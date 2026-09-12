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

        // Fast path: writer cursor is byte-aligned -> push whole bytes directly.
        if self.nbits == 0 {
            let nbytes = remaining >> 3;
            self.out.extend_from_slice(&data[..nbytes]);
            i = nbytes;
            remaining -= nbytes * 8;
        } else {
            while remaining >= 8 {
                self.write_bits(data[i] as u64, 8);
                i += 1;
                remaining -= 8;
            }
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

    pub fn bits_read(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn read_bit(&mut self) -> u32 {
        let byte = self.data[self.pos >> 3];
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
            let byte = self.data[byte_idx] as u64;
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

    pub fn read_array(&mut self, dst: &mut [u8], count_bits: usize) {
        let mut remaining = count_bits;
        let mut i = 0;

        if self.pos & 7 == 0 {
            let nbytes = remaining >> 3;
            let start = self.pos >> 3;
            dst[..nbytes].copy_from_slice(&self.data[start..start + nbytes]);
            self.pos += nbytes * 8;
            i = nbytes;
            remaining -= nbytes * 8;
        } else {
            while remaining >= 64 {
                let word = self.read_bits(64);
                dst[i..i + 8].copy_from_slice(&word.to_be_bytes());
                i += 8;
                remaining -= 64;
            }
            while remaining >= 8 {
                dst[i] = self.read_bits(8) as u8;
                i += 1;
                remaining -= 8;
            }
        }

        if remaining > 0 {
            let v = self.read_bits(remaining as u32) as u8;
            dst[i] = v << (8 - remaining);
        }
    }
}
