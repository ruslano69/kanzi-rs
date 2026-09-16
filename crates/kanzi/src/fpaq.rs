// Port of kanzi-go's FPAQCodec (entropy/FPAQCodec.go) -- the adaptive
// bit-level range codec (fpaq0-derived) used as the L6 entropy (FPAQ).
// Both roles are fully ported.
//
// Fidelity notes:
// - All range/state arithmetic uses wrapping semantics: Go's int/uint64
//   arithmetic wraps silently (including `uint64(negativePr)` casts and
//   `low <<= 32` truncation), Rust panics in debug, so every hot-path op is
//   an explicit wrapping_* or wrapping `as` cast.
// - The encoder's ctxIdx field is dead upstream (initialized, never used by
//   Write which threads contexts locally) and is not ported.
// - Only the V2 decode path is ported (decodeBitV1 serves bitstream versions
//   < 4; this project is v7-only and Go takes the V2 branch for >= 4).
// - Probe tables persist across chunks within one block (adaptive, like Go:
//   fresh instances are created per block by the container on both sides).

use crate::bitio::{BitReader, BitWriter};

const FPAQ_PSCALE: i64 = 1 << 16;
const FPAQ_DEFAULT_CHUNK_SIZE: usize = 4 * 1024 * 1024;
const FPAQ_ENTROPY_TOP: u64 = 0x00FF_FFFF_FFFF_FFFF;
const FPAQ_MASK_0_56: u64 = 0x00FF_FFFF_FFFF_FFFF;
const FPAQ_MASK_0_24: u64 = 0x0000_0000_00FF_FFFF;
const FPAQ_MASK_0_32: u64 = 0x0000_0000_FFFF_FFFF;

fn write_var_int(bw: &mut BitWriter, mut value: u32) {
    while value >= 128 {
        bw.write_bits((0x80 | (value & 0x7F)) as u64, 8);
        value >>= 7;
    }
    bw.write_bits(value as u64, 8);
}

fn read_var_int(br: &mut BitReader) -> u32 {
    let mut res: u32 = 0;
    let mut shift: u32 = 0;

    for _ in 0..4 {
        let value = br.read_bits(8) as u32;
        res |= (value & 0x7F) << shift;

        if value < 128 {
            return res;
        }

        shift += 7;
    }

    let value = br.read_bits(8) as u32;
    res | ((value & 0x0F) << 28)
}

fn init_probs() -> [[i32; 256]; 4] {
    [[FPAQ_PSCALE as i32 >> 1; 256]; 4]
}

/// One FPAQ bit, fully inlined (port of kanzi-cpp's
/// `FPAQEncoder::encodeBit`, which mutates `_low`/`_high` in place).
/// Operates on the `low`/`high` locals; spills them to `self` only on the
/// rare flush path. Arithmetic is bit-exact with the previous
/// `encode_bit_inlined` version (wrapping `u64` range ops, `i32` prob
/// updates with Go-style wrapping).
macro_rules! encode_bit {
    ($slf:expr, $low:ident, $high:ident, $ptab:expr, $ctx:expr, $is_zero:expr) => {{
        let pr = $slf.probs[$ptab][$ctx];
        let split = (($high.wrapping_sub($low) >> 8).wrapping_mul(pr as u64)) >> 8;

        if $is_zero {
            $low = $low.wrapping_add(split.wrapping_add(1));
            $slf.probs[$ptab][$ctx] = pr.wrapping_sub(pr >> 6);
        } else {
            $high = $low.wrapping_add(split);
            let dec = pr.wrapping_sub(FPAQ_PSCALE as i32).wrapping_add(64) >> 6;
            $slf.probs[$ptab][$ctx] = pr.wrapping_sub(dec);
        }

        if ($low ^ $high) < (1 << 24) {
            $slf.low = $low;
            $slf.high = $high;
            $slf.flush();
            $low = $slf.low;
            $high = $slf.high;
        }
    }};
}

pub struct FpaqEncoder {
    low: u64,
    high: u64,
    buffer: Vec<u8>,
    index: usize,
    probs: [[i32; 256]; 4],
    disposed: bool,
}

impl FpaqEncoder {
    pub fn new() -> Self {
        FpaqEncoder {
            low: 0,
            high: FPAQ_ENTROPY_TOP,
            buffer: Vec::new(),
            index: 0,
            probs: init_probs(),
            disposed: false,
        }
    }

    fn flush(&mut self) {
        if self.index + 4 > self.buffer.len() {
            self.buffer.resize(self.index + 4, 0);
        }

        let bytes = ((self.high >> 24) as u32).to_be_bytes();
        self.buffer[self.index..self.index + 4].copy_from_slice(&bytes);
        self.index += 4;
        self.low = self.low.wrapping_shl(32);
        self.high = self.high.wrapping_shl(32) | FPAQ_MASK_0_32;
    }

    /// Encodes the block, chunked at 4MB like Go (range state carries across
    /// chunks). Returns bytes of input consumed (= len).
    pub fn write(&mut self, block: &[u8], bw: &mut BitWriter) -> usize {
        let count = block.len();

        // Go returns an error above 1GB; our container caps blocks at 1GB,
        // so this is unreachable in practice -- loud contract either way.
        assert!(count <= 1 << 30, "FPAQ codec: invalid block size");

        let end = count;
        let mut start_chunk = 0usize;

        while start_chunk < end {
            let chunk_size = FPAQ_DEFAULT_CHUNK_SIZE.min(end - start_chunk);
            let size = FPAQ_DEFAULT_CHUNK_SIZE.min(count);
            let extra = (size >> 3).max(size.min(1 << 16));
            let buf_size = (size + extra).max(1024);

            if self.buffer.len() < buf_size {
                self.buffer = vec![0u8; buf_size];
            }

            self.index = 0;
            let buf = &block[start_chunk..start_chunk + chunk_size];
            let mut ptab = 0usize;
            let mut low = self.low;
            let mut high = self.high;

            for &val in buf {
                let bits = val as u32 + 256;

                // Fully unrolled 8-bit walk, mirroring kanzi-cpp's 8
                // explicit encodeBit calls: bit 7 uses context 1 in the
                // current table, bits 6..0 use the decoded-prefix context.
                encode_bit!(self, low, high, ptab, 1, val & 0x80 == 0);
                encode_bit!(self, low, high, ptab, (bits >> 7) as usize, val & 0x40 == 0);
                encode_bit!(self, low, high, ptab, (bits >> 6) as usize, val & 0x20 == 0);
                encode_bit!(self, low, high, ptab, (bits >> 5) as usize, val & 0x10 == 0);
                encode_bit!(self, low, high, ptab, (bits >> 4) as usize, val & 0x08 == 0);
                encode_bit!(self, low, high, ptab, (bits >> 3) as usize, val & 0x04 == 0);
                encode_bit!(self, low, high, ptab, (bits >> 2) as usize, val & 0x02 == 0);
                encode_bit!(self, low, high, ptab, (bits >> 1) as usize, val & 0x01 == 0);

                ptab = (val >> 6) as usize;
            }

            self.low = low;
            self.high = high;

            write_var_int(bw, self.index as u32);
            bw.write_array(&self.buffer[..self.index], 8 * self.index);
            start_chunk += chunk_size;

            if start_chunk < end {
                bw.write_bits(self.low | FPAQ_MASK_0_24, 56);
            }
        }

        count
    }

    /// Idempotent final flush of the range state (must be called once per
    /// block, like Go's Dispose).
    pub fn dispose(&mut self, bw: &mut BitWriter) {
        if self.disposed {
            return;
        }

        self.disposed = true;
        bw.write_bits(self.low | FPAQ_MASK_0_24, 56);
    }
}

/// One FPAQ decode bit, fully inlined (port of Go's decodeBitV2). The
/// context `ctx` flows from one bit to the next within a byte, so the
/// caller's 8 step invocations stay strictly sequential.
macro_rules! decode_bit {
    ($slf:expr, $low:ident, $high:ident, $current:ident, $ptab:expr, $ctx:ident) => {{
        let pr = $slf.probs[$ptab][$ctx as usize];
        let split = (($high.wrapping_sub($low) >> 8).wrapping_mul(pr as u64) >> 8)
            .wrapping_add($low);

        if split >= $current {
            $high = split;
            $slf.probs[$ptab][$ctx as usize] = pr.wrapping_sub(
                pr.wrapping_sub(FPAQ_PSCALE as i32).wrapping_add(64) >> 6,
            );
            $ctx = $ctx.wrapping_add($ctx).wrapping_add(1);
        } else {
            $low = split.wrapping_add(1);
            $slf.probs[$ptab][$ctx as usize] = pr.wrapping_sub(pr >> 6);
            $ctx = $ctx.wrapping_add($ctx);
        }

        if ($low ^ $high) < (1 << 24) {
            // Slow path: sync and refill.
            $slf.low = $low;
            $slf.high = $high;
            $slf.current = $current;
            $slf.read();
            $low = $slf.low;
            $high = $slf.high;
            $current = $slf.current;
        }
    }};
}

pub struct FpaqDecoder {
    low: u64,
    high: u64,
    current: u64,
    buffer: Vec<u8>,
    buf_limit: usize,
    index: usize,
    probs: [[i32; 256]; 4],
}

impl FpaqDecoder {
    pub fn new() -> Self {
        FpaqDecoder {
            low: 0,
            high: FPAQ_ENTROPY_TOP,
            current: 0,
            buffer: Vec::new(),
            buf_limit: 0,
            index: 0,
            probs: init_probs(),
        }
    }

    fn read(&mut self) {
        self.low = self.low.wrapping_shl(32) & FPAQ_MASK_0_56;
        self.high = (self.high.wrapping_shl(32) | FPAQ_MASK_0_32) & FPAQ_MASK_0_56;

        if self.index + 4 > self.buf_limit {
            self.current = self.current.wrapping_shl(32) & FPAQ_MASK_0_56;
            self.index = self.buf_limit + 1;
            return;
        }

        let val =
            u32::from_be_bytes(self.buffer[self.index..self.index + 4].try_into().unwrap())
                as u64;
        self.current = (self.current.wrapping_shl(32) | val) & FPAQ_MASK_0_56;
        self.index += 4;
    }

    /// Decodes into `block` (V2 path only). Returns bytes decoded.
    pub fn read_block(&mut self, br: &mut BitReader, block: &mut [u8]) -> Result<usize, String> {
        let count = block.len();

        if count > 1 << 30 {
            return Err("FPAQ codec: invalid block size".to_string());
        }

        let end = count;
        let mut start_chunk = 0usize;

        while start_chunk < end {
            let sz_bytes = read_var_int(br) as usize;

            if sz_bytes >= 2 * block.len() {
                return Err(format!("FPAQ codec: invalid chunk size ({})", sz_bytes));
            }

            let buf_size = (sz_bytes + (sz_bytes >> 2)).max(1024);

            if self.buffer.len() < buf_size {
                self.buffer = vec![0u8; buf_size];
            }

            self.current = br.read_bits(56);

            // Ensure deterministic refill words past payload end without
            // clearing the whole tail (mirrors Go).
            if sz_bytes < self.buffer.len() {
                let guard_end = (sz_bytes + 8).min(self.buffer.len());
                self.buffer[sz_bytes..guard_end].fill(0);
            }

            br.read_array(&mut self.buffer[..sz_bytes], 8 * sz_bytes);
            self.buf_limit = sz_bytes;
            self.index = 0;
            let chunk_size = FPAQ_DEFAULT_CHUNK_SIZE.min(end - start_chunk);
            let buf = &mut block[start_chunk..start_chunk + chunk_size];

            let mut ptab = 0usize;
            let mut low = self.low;
            let mut high = self.high;
            let mut current = self.current;

            for slot in buf.iter_mut() {
                let mut ctx = 1u8;

                // Unrolled 8x decodeBitV2 (was `for _ in 0..8`).
                decode_bit!(self, low, high, current, ptab, ctx);
                decode_bit!(self, low, high, current, ptab, ctx);
                decode_bit!(self, low, high, current, ptab, ctx);
                decode_bit!(self, low, high, current, ptab, ctx);
                decode_bit!(self, low, high, current, ptab, ctx);
                decode_bit!(self, low, high, current, ptab, ctx);
                decode_bit!(self, low, high, current, ptab, ctx);
                decode_bit!(self, low, high, current, ptab, ctx);

                *slot = ctx;
                // NOTE: Go also writes this.ctx = ctx (field used only by the
                // V1 path); V2 keeps it local, like here.

                if self.index > sz_bytes {
                    self.low = low;
                    self.high = high;
                    self.current = current;
                    return Err("FPAQ codec: invalid bitstream".to_string());
                }

                ptab = ((ctx & 0xFF) >> 6) as usize;
            }

            self.low = low;
            self.high = high;
            self.current = current;

            if self.index > sz_bytes {
                return Err("FPAQ codec: invalid bitstream".to_string());
            }

            start_chunk += chunk_size;
        }

        Ok(count)
    }
}
