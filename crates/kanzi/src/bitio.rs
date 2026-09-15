// MSB-first bit reader/writer matching kanzi-go's bitstream.DefaultInputBitStream
// / DefaultOutputBitStream semantics. Correctness-first (no bulk/aligned fast
// paths) since this is the container/header path, not a symbol-decode hot loop.

pub struct BitWriter {
    out: Vec<u8>,
    cur: u8,
    nbits: u32, // bits already placed in `cur`, from the MSB side (0..7)
}

impl BitWriter {
    pub fn new() -> Self {
        BitWriter {
            out: Vec::new(),
            cur: 0,
            nbits: 0,
        }
    }

    /// Writes the low `n` bits of `value` (1..=64), MSB first.
    pub fn write_bits(&mut self, value: u64, n: u32) {
        for i in (0..n).rev() {
            let bit = ((value >> i) & 1) as u8;
            self.cur |= bit << (7 - self.nbits);
            self.nbits += 1;

            if self.nbits == 8 {
                self.out.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
            }
        }
    }

    pub fn write_bit(&mut self, bit: u32) {
        self.write_bits(bit as u64, 1);
    }

    /// Writes `count_bits` bits from `data` (MSB-first per byte); a trailing
    /// partial byte contributes its top bits, matching kanzi-go's WriteArray.
    pub fn write_array(&mut self, data: &[u8], count_bits: usize) {
        let mut remaining = count_bits;
        let mut i = 0;

        if remaining == 0 {
            return;
        }

        let nbytes = remaining >> 3;
        self.out.reserve(nbytes + 8);

        // Fast path: writer cursor is byte-aligned -> push whole bytes directly.
        if self.nbits == 0 {
            self.out.extend_from_slice(&data[..nbytes]);
            i = nbytes;
            remaining -= nbytes * 8;
        } else {
            // Unaligned: one byte out per byte in. Doing this through
            // `write_bits` (8 bits -> 8 inner iterations) was ~8x slower and
            // dominated container framing, where the block-length prefix is
            // almost never a multiple of 8 bits.
            let nb = self.nbits;

            for _ in 0..nbytes {
                let b = data[i];
                self.out.push(self.cur | (b >> nb));
                self.cur = b << (8 - nb);
                i += 1;
            }

            remaining -= nbytes * 8;
        }

        if remaining > 0 {
            let top = data[i] >> (8 - remaining);
            self.write_bits(top as u64, remaining as u32);
        }
    }

    /// Total bits written so far.
    pub fn bit_len(&self) -> u64 {
        (self.out.len() as u64) * 8 + self.nbits as u64
    }

    /// Flushes the partial trailing byte (zero-padded) and returns the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.out.push(self.cur);
        }
        self.out
    }

    /// Like `finish`, but also returns the exact (pre-padding) bit count --
    /// needed to splice this writer's output into another BitWriter at the
    /// bit level without introducing the zero-padding as real content.
    pub fn finish_with_len(self) -> (Vec<u8>, u64) {
        let len = self.bit_len();
        (self.finish(), len)
    }
}

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // absolute bit offset
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// Anchors a fresh reader at an arbitrary absolute bit position within
    /// `data`, instead of the start. Used to decode a container block
    /// in-place from a shared byte buffer without first slicing/copying
    /// out its (not necessarily byte-aligned) bit range -- e.g. one worker
    /// thread per block in the container's parallel block decode, each
    /// independently reading from `data` starting at that block's own
    /// framed-length-prefix-relative offset (see `container::decode`).
    pub fn at_bit_pos(data: &'a [u8], pos: usize) -> Self {
        BitReader { data, pos }
    }

    pub fn bits_read(&self) -> usize {
        self.pos
    }

    /// Advances the cursor by `n` bits without reading anything -- for
    /// skipping over a block's payload during a first pass that only
    /// needs to locate block boundaries (container::decode's parallel
    /// split), not decode their content. O(1) instead of O(n/64) since
    /// nothing needs to be fetched or shifted.
    #[inline]
    pub fn skip_bits(&mut self, n: usize) {
        self.pos += n;
    }

    /// Reads past the end of `data` return 0 rather than panicking (see
    /// the module-level fidelity note): every decoder in this project
    /// assumes wire-format-internal size fields (chunk sizes, symbol
    /// counts, etc.) are trustworthy, matching Go's own decoders. For a
    /// corrupted stream those fields can be arbitrary, and following them
    /// naively can walk this cursor past the actual file content -- e.g.
    /// an unbounded "read bits until a 1" loop (Huffman's Exp-Golomb
    /// decode) landing in a long run of corrupted zero bits. Reading zeros
    /// past the end keeps that deterministic and panic-free; the garbage
    /// result still gets caught by whatever downstream validation the
    /// format has (an end-of-chunk size check, a checksum, ...) instead of
    /// crashing before ever reaching it.
    #[inline]
    fn byte_at(&self, idx: usize) -> u8 {
        if idx < self.data.len() {
            self.data[idx]
        } else {
            0
        }
    }

    #[inline]
    pub fn read_bit(&mut self) -> u32 {
        let byte = self.byte_at(self.pos >> 3);
        let bit = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        bit as u32
    }

    #[inline]
    pub fn read_bits(&mut self, n: u32) -> u64 {
        let mut result: u64 = 0;
        let mut remaining = n;

        while remaining > 0 {
            let byte_idx = self.pos >> 3;
            let bit_off = (self.pos & 7) as u32;
            let avail_in_byte = 8 - bit_off;
            let take = remaining.min(avail_in_byte);
            let byte = self.byte_at(byte_idx) as u64;
            let shift = avail_in_byte - take;
            let mask: u64 = if take == 64 {
                u64::MAX
            } else {
                (1u64 << take) - 1
            };
            result = (result << take) | ((byte >> shift) & mask);
            self.pos += take as usize;
            remaining -= take;
        }

        result
    }

    /// Reads `count_bits` bits into `dst`. `count_bits` past `8*dst.len()`
    /// is a caller bug for trusted call sites, but several decoders derive
    /// it from an untrusted wire-format length field (a chunk byte count,
    /// say) against a `dst` sized for the *expected* size -- so instead of
    /// trusting the caller, this clamps the actual write to `dst`'s real
    /// size (silently dropping any excess, never writing past it) and
    /// still advances the cursor by the full `count_bits` requested, as if
    /// the read had fully succeeded. Reading past the end of `data` itself
    /// returns zeros (see `byte_at`), for the same reason.
    pub fn read_array(&mut self, dst: &mut [u8], count_bits: usize) {
        let bits_to_write = count_bits.min(dst.len() * 8);
        let extra_bits = count_bits - bits_to_write;
        let mut remaining = bits_to_write;
        let mut i = 0;

        if self.pos & 7 == 0 {
            let nbytes = remaining >> 3;
            let start = self.pos >> 3;
            let avail = self.data.len().saturating_sub(start).min(nbytes);
            dst[..avail].copy_from_slice(&self.data[start..start + avail]);

            if avail < nbytes {
                dst[avail..nbytes].fill(0);
            }

            self.pos += nbytes * 8;
            i = nbytes;
            remaining -= nbytes * 8;
        } else {
            // Unaligned: combine each output byte from two adjacent input
            // bytes. The previous implementation went through `read_bits`
            // (an internal multi-step loop) per 8 bits, which was far more
            // work per byte.
            let bit_off = self.pos & 7;
            let mut byte_idx = self.pos >> 3;

            while remaining >= 8 {
                let b0 = self.byte_at(byte_idx);
                let b1 = self.byte_at(byte_idx + 1);
                dst[i] = (b0 << bit_off) | (b1 >> (8 - bit_off));
                byte_idx += 1;
                i += 1;
                remaining -= 8;
            }

            self.pos = byte_idx * 8 + bit_off;
        }

        if remaining > 0 {
            let v = self.read_bits(remaining as u32) as u8;
            dst[i] = v << (8 - remaining);
        }

        // Bits that didn't fit in `dst`: still consume them from the
        // stream (matching a "successful" read's cursor movement) without
        // writing anywhere.
        self.pos += extra_bits;
    }
}
