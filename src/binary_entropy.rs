// Port of kanzi-go's BinaryEntropyEncoder/Decoder (entropy/BinaryEntropyCodec.go)
// -- a generic binary arithmetic coder driven by an external probability
// predictor. Used directly by CM (cm.rs) and TPAQ/TPAQX (tpaq.rs); unlike
// FPAQCodec (fpaq.rs), which bakes its own fixed context-table predictor
// into the coder for speed, this coder is fully generic over any
// `Predictor`, matching Go's `kanzi.Predictor` interface (Get()/Update()).
//
// Fidelity notes (see fpaq.rs for the same notes, this is the same shape):
// - All range/state arithmetic uses wrapping semantics, matching Go's
//   defined two's-complement wraparound for signed and unsigned ints.
// - Chunking mirrors Go exactly: blocks under 64MiB are encoded as a single
//   chunk (length = count); only larger blocks split (count>>3 or count>>4).
//   For this project's block sizes (<=1GiB, normally a few MiB) this means
//   a single chunk in practice, but the general form is ported for parity.
// - The exact bit-write/read order per chunk (varint length, then a 56-bit
//   `low`/`current` seed -- written only *between* chunks by the encoder,
//   via `flush`/`Dispose`, but *always* read fresh by the decoder at the
//   start of each chunk -- then the chunk's own flushed bytes) looks
//   mismatched read against write in isolation; it is not a bug. The
//   arithmetic coder's `current` value is a continuously-updated window
//   into the bit sequence, not a per-chunk-aligned quantity, and the total
//   bit budget matches exactly once `Dispose`'s trailing 56-bit write (one
//   per block, unconditional) is counted alongside the (chunks-1) internal
//   56-bit boundary writes. This is mechanically transcribed from Go/fpaq.rs
//   rather than re-derived, and is verified byte-exact against the real Go
//   decoder/encoder (see the l7 container tests).

use crate::bitio::{BitReader, BitWriter};

const ENTROPY_TOP: u64 = 0x00FF_FFFF_FFFF_FFFF;
const MASK_0_56: u64 = 0x00FF_FFFF_FFFF_FFFF;
const MASK_0_24: u64 = 0x0000_0000_00FF_FFFF;
const MASK_0_32: u64 = 0x0000_0000_FFFF_FFFF;
const MAX_BLOCK: usize = 1 << 30;
const MAX_CHUNK: usize = 1 << 26;

pub trait Predictor {
    /// Probability that the next bit is 1, in [0..4095] (12-bit fixed
    /// point). `&mut self` because some predictors (e.g. CM) cache
    /// state read here for the following `update()` call, like Go's.
    fn get(&mut self) -> i32;
    fn update(&mut self, bit: u8);
}

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

fn chunk_length(count: usize) -> usize {
    if count >= MAX_CHUNK {
        if count < 8 * MAX_CHUNK {
            count >> 3
        } else {
            count >> 4
        }
    } else if count < 64 {
        64
    } else {
        count
    }
}

pub struct BinaryEntropyEncoder<P: Predictor> {
    predictor: P,
    low: u64,
    high: u64,
    buffer: Vec<u8>,
    index: usize,
    disposed: bool,
}

impl<P: Predictor> BinaryEntropyEncoder<P> {
    pub fn new(predictor: P) -> Self {
        BinaryEntropyEncoder { predictor, low: 0, high: ENTROPY_TOP, buffer: Vec::new(), index: 0, disposed: false }
    }

    fn flush(&mut self) {
        if self.index + 4 > self.buffer.len() {
            self.buffer.resize(self.index + 4, 0);
        }

        let bytes = ((self.high >> 24) as u32).to_be_bytes();
        self.buffer[self.index..self.index + 4].copy_from_slice(&bytes);
        self.index += 4;
        self.low = self.low.wrapping_shl(32);
        self.high = self.high.wrapping_shl(32) | MASK_0_32;
    }

    #[inline]
    fn encode_bit(&mut self, bit: u8, pred: i32) {
        let split = ((self.high.wrapping_sub(self.low) >> 4).wrapping_mul(pred as u64)) >> 8;

        if bit == 0 {
            self.low = self.low.wrapping_add(split.wrapping_add(1));
        } else {
            self.high = self.low.wrapping_add(split);
        }

        self.predictor.update(bit);

        if (self.low ^ self.high) < (1 << 24) {
            self.flush();
        }
    }

    /// Encodes the block, chunked exactly like Go (range state carries
    /// across chunks). Returns bytes of input consumed (= len).
    pub fn write(&mut self, block: &[u8], bw: &mut BitWriter) -> Result<usize, String> {
        let count = block.len();

        if count > MAX_BLOCK {
            return Err("Binary entropy codec: Invalid block size parameter (max is 1<<30)".to_string());
        }

        let mut start_chunk = 0usize;
        let end = count;
        let length = chunk_length(count);
        let extra = (length >> 3).max(length.min(1 << 16));
        let buf_size = length + extra;

        if self.buffer.len() < buf_size {
            self.buffer = vec![0u8; buf_size];
        }

        while start_chunk < end {
            let chunk_size = length.min(end - start_chunk);
            let buf = &block[start_chunk..start_chunk + chunk_size];
            self.index = 0;

            for &val in buf {
                for shift in (0..8u8).rev() {
                    let pred = self.predictor.get();
                    self.encode_bit((val >> shift) & 1, pred);
                }
            }

            write_var_int(bw, self.index as u32);
            bw.write_array(&self.buffer[..self.index], 8 * self.index);
            start_chunk += chunk_size;

            if start_chunk < end {
                bw.write_bits(self.low | MASK_0_24, 56);
            }
        }

        Ok(count)
    }

    /// Idempotent final flush of the range state (must be called once per
    /// block, like Go's Dispose).
    pub fn dispose(&mut self, bw: &mut BitWriter) {
        if self.disposed {
            return;
        }

        self.disposed = true;
        bw.write_bits(self.low | MASK_0_24, 56);
    }
}

pub struct BinaryEntropyDecoder<P: Predictor> {
    predictor: P,
    low: u64,
    high: u64,
    current: u64,
    buffer: Vec<u8>,
    index: usize,
    buf_limit: usize,
}

impl<P: Predictor> BinaryEntropyDecoder<P> {
    pub fn new(predictor: P) -> Self {
        BinaryEntropyDecoder {
            predictor,
            low: 0,
            high: ENTROPY_TOP,
            current: 0,
            buffer: Vec::new(),
            index: 0,
            buf_limit: 0,
        }
    }

    // Called on every renormalization (roughly once per 4 bytes of
    // *compressed* input consumed) -- the one checked-indexing site left in
    // the shared decode loop that CM/TPAQ/TPAQX all drive (their `get()`/
    // `update()` are already bounds-check-eliminated; see cm.rs, tpaq.rs).
    // `self.buffer` is grown to at least `buf_limit` (`sz_bytes`) bytes
    // before any chunk's bits are read (`read_block`'s two `if
    // self.buffer.len() < ...` resizes, one for `buf_size` up front, one
    // for `sz_bytes` per chunk) and never shrunk afterward, so
    // `buf_limit <= self.buffer.len()` holds for the lifetime of a
    // `read_block` call; the early return above is exactly the
    // `index + 4 > buf_limit` case, so past it `index + 4 <= buf_limit <=
    // self.buffer.len()`.
    fn read(&mut self) {
        self.low = self.low.wrapping_shl(32) & MASK_0_56;
        self.high = (self.high.wrapping_shl(32) | MASK_0_32) & MASK_0_56;

        if self.index + 4 > self.buf_limit {
            self.current = self.current.wrapping_shl(32) & MASK_0_56;
            self.index = self.buf_limit + 1;
            return;
        }

        debug_assert!(self.index + 4 <= self.buffer.len());
        let val = unsafe {
            u32::from_be_bytes(
                self.buffer
                    .get_unchecked(self.index..self.index + 4)
                    .try_into()
                    .unwrap(),
            )
        } as u64;
        self.current = (self.current.wrapping_shl(32) | val) & MASK_0_56;
        self.index += 4;
    }

    #[inline]
    fn decode_bit(&mut self, pred: i32) -> u8 {
        let split = (((self.high.wrapping_sub(self.low) >> 4).wrapping_mul(pred as u64)) >> 8).wrapping_add(self.low);
        let bit;

        if split >= self.current {
            bit = 1;
            self.high = split;
            self.predictor.update(1);
        } else {
            bit = 0;
            self.low = split.wrapping_add(1);
            self.predictor.update(0);
        }

        if (self.low ^ self.high) < (1 << 24) {
            self.read();
        }

        bit
    }

    /// Decodes into `block`. Returns bytes decoded.
    pub fn read_block(&mut self, br: &mut BitReader, block: &mut [u8]) -> Result<usize, String> {
        let count = block.len();

        if count > MAX_BLOCK {
            return Err("Binary entropy codec: Invalid block size parameter (max is 1<<30)".to_string());
        }

        let mut start_chunk = 0usize;
        let end = count;
        let length = chunk_length(count);
        let buf_size = length + (length >> 3);

        if self.buffer.len() < buf_size {
            self.buffer = vec![0u8; buf_size];
        }

        while start_chunk < end {
            let chunk_size = length.min(end - start_chunk);
            let sz_bytes = read_var_int(br) as u64;
            let max_encoded_size = ((chunk_size as u64) << 5).min(u64::MAX >> 3);

            if sz_bytes > max_encoded_size {
                return Err("Binary entropy codec: Invalid bitstream".to_string());
            }

            let sz_bytes = sz_bytes as usize;

            if self.buffer.len() < sz_bytes {
                self.buffer = vec![0u8; sz_bytes];
            }

            self.current = br.read_bits(56);

            if sz_bytes != 0 {
                br.read_array(&mut self.buffer[..sz_bytes], 8 * sz_bytes);
            }

            self.buf_limit = sz_bytes;
            self.index = 0;
            let buf = &mut block[start_chunk..start_chunk + chunk_size];

            for slot in buf.iter_mut() {
                let mut v: u8 = 0;

                for _ in 0..8 {
                    let pred = self.predictor.get();
                    let bit = self.decode_bit(pred);
                    v = (v << 1) | bit;
                }

                *slot = v;

                if self.index > sz_bytes {
                    return Err("Binary entropy codec: Invalid bitstream".to_string());
                }
            }

            start_chunk += chunk_size;
        }

        Ok(count)
    }
}
