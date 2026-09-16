// MSB-first bit reader/writer matching kanzi-go's bitstream.DefaultInputBitStream
// / DefaultOutputBitStream semantics. Bit-level reads and writes stay simple;
// the bulk array paths (`read_array`/`write_array`) are the throughput-relevant
// ones, since every block payload and entropy chunk moves through them.

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
            // Unaligned: one byte out per byte in, the pending `nb` bits in
            // front. Container framing lands here for every block (the
            // block-length prefix is almost never a multiple of 8 bits), so
            // shift 8 bytes at a time through a big-endian u64; byte by byte
            // this ran ~1 GB/s and dominated encoding at levels 0-1.
            let nb = self.nbits;

            while i + 8 <= nbytes {
                let w = u64::from_be_bytes(data[i..i + 8].try_into().unwrap());
                let out = ((self.cur as u64) << 56) | (w >> nb);
                self.out.extend_from_slice(&out.to_be_bytes());
                self.cur = ((w & 0xFF) as u8) << (8 - nb);
                i += 8;
            }

            while i < nbytes {
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

    /// Writes the complete bytes accumulated so far to `w`, in pieces of at
    /// most 4 MiB, and clears them (keeping the allocation), leaving only a
    /// pending partial byte. Lets a stream be framed incrementally instead of
    /// assembled in memory. `bit_len` keeps counting only the undrained bits.
    pub fn drain_to<W: std::io::Write>(&mut self, w: &mut W) -> std::io::Result<usize> {
        for chunk in self.out.chunks(4 << 20) {
            w.write_all(chunk)?;
        }

        let n = self.out.len();
        self.out.clear();
        Ok(n)
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
            // Unaligned: each output byte straddles two input bytes. Shift 8
            // bytes at a time through a big-endian u64, with the following
            // input byte supplying the low bits, while 9 input bytes remain;
            // then finish byte by byte, where `byte_at` supplies zeros past
            // the end. A block's payload starts after a variable-length bit
            // header, so this path carries every container payload read
            // (raw/transformed copies, NONE entropy, and the entropy
            // decoders' own chunk reads); byte-at-a-time it ran ~850 MB/s.
            let bit_off = self.pos & 7;
            let mut byte_idx = self.pos >> 3;

            while remaining >= 64 && byte_idx + 9 <= self.data.len() {
                let w = u64::from_be_bytes(self.data[byte_idx..byte_idx + 8].try_into().unwrap());
                let low = self.data[byte_idx + 8] as u64;
                let v = (w << bit_off) | (low >> (8 - bit_off));
                dst[i..i + 8].copy_from_slice(&v.to_be_bytes());
                byte_idx += 8;
                i += 8;
                remaining -= 64;
            }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// `write_array` against writing the same bits one byte at a time with
    /// `write_bits`, from every starting bit offset, across the 8-byte fast
    /// path's boundaries and with a trailing partial byte.
    #[test]
    fn write_array_matches_bytewise_write_bits() {
        let data: Vec<u8> = (0..300u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();

        for lead_bits in 0..8u32 {
            for count_bits in [0usize, 7, 8, 63, 64, 65, 71, 72, 80, 800, 1601, 2400] {
                let mut fast = BitWriter::new();
                fast.write_bits(0b1011_0101, lead_bits.max(1));
                fast.write_array(&data, count_bits);

                let mut slow = BitWriter::new();
                slow.write_bits(0b1011_0101, lead_bits.max(1));

                for b in &data[..count_bits / 8] {
                    slow.write_bits(*b as u64, 8);
                }

                if count_bits % 8 != 0 {
                    slow.write_bits((data[count_bits / 8] >> (8 - count_bits % 8)) as u64, (count_bits % 8) as u32);
                }

                let ctx = format!("lead_bits={lead_bits} count_bits={count_bits}");
                assert_eq!(fast.bit_len(), slow.bit_len(), "{ctx}");
                assert_eq!(fast.finish(), slow.finish(), "{ctx}");
            }
        }
    }

    /// `read_array` against the simplest possible reference -- `read_bits`
    /// one byte at a time -- at every bit offset, for lengths that cross
    /// the 8-byte fast path's boundaries, run past the end of the data, are
    /// not whole bytes, or exceed `dst`.
    #[test]
    fn read_array_matches_bytewise_read_bits() {
        let mut state = 0x1234_5678_9abc_def1u64;
        let data: Vec<u8> = (0..300)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();

        for start_bit in 0..16 {
            for count_bits in [0usize, 7, 8, 63, 64, 65, 71, 72, 80, 800, 1601, 2400, 2500] {
                for dst_len in [count_bits / 8, count_bits.div_ceil(8), 40] {
                    let mut fast = BitReader::at_bit_pos(&data, start_bit);
                    let mut got = vec![0xAAu8; dst_len];
                    fast.read_array(&mut got, count_bits);

                    let mut slow = BitReader::at_bit_pos(&data, start_bit);
                    let mut want = vec![0xAAu8; dst_len];
                    let bits = count_bits.min(dst_len * 8);

                    for b in want.iter_mut().take(bits / 8) {
                        *b = slow.read_bits(8) as u8;
                    }

                    if bits % 8 != 0 {
                        want[bits / 8] = (slow.read_bits((bits % 8) as u32) as u8) << (8 - bits % 8);
                    }

                    let ctx = format!("start_bit={start_bit} count_bits={count_bits} dst_len={dst_len}");
                    assert_eq!(got, want, "{ctx}");
                    assert_eq!(fast.bits_read(), start_bit + count_bits, "{ctx}: cursor");
                }
            }
        }
    }
}
