// Port of the parts of kanzi-go's io/CompressedStream.go container format
// needed for levels 1-4 (no checksum, single job). Byte-exact /
// wire-compatible with the real kanzi CLI on the supported sequences.

use crate::alias;
use crate::ans::{AnsDecoder, AnsEncoder};
use crate::binary_entropy::{BinaryEntropyDecoder, BinaryEntropyEncoder};
use crate::bitio::{BitReader, BitWriter};
use crate::bwt::Bwt;
use crate::cm::CmPredictor;
use crate::datatype::DataType;
use crate::exe;
use crate::fpaq::{FpaqDecoder, FpaqEncoder};
use crate::fsd;
use crate::huffman_dec::HuffmanDecoderV6;
use crate::huffman_enc::HuffmanEncoder;
use crate::lzp::LzpCodec;
use crate::lzx;
use crate::lzx::LzxCodec;
use crate::rlt;
use crate::rolz::RolzCodec;
use crate::sbrt::Sbrt;
use crate::srt::Srt;
use crate::text_codec;
use crate::text_codec1;
use crate::tpaq::TpaqPredictor;
use crate::utf;
use crate::xxhash::{XxHash32, XxHash64};
use crate::zrlt;

/// Number of block worker threads (encode and decode):
/// `available_parallelism()` capped at `max`, unless `KANZI_JOBS` overrides
/// it (the equivalent of kanzi-cpp's `-j`). Pinning this to 1 is what lets a
/// per-block speed comparison be separated from a thread-scaling comparison.
fn worker_count(max: usize) -> usize {
    let avail = std::env::var("KANZI_JOBS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        });
    avail.min(max)
}

const BITSTREAM_TYPE: u64 = 0x4B414E5A; // "KANZ"
const BITSTREAM_FORMAT_VERSION: u64 = 7;
const HASH: u32 = 0x1E35A7BD;
const LZX_TYPE: u64 = 16;
const LZ_TYPE: u64 = 3;
const DNA_TYPE: u64 = 19;
const TEXT_TYPE: u64 = 10;
const UTF_TYPE: u64 = 17;
const PACK_TYPE: u64 = 18;
const MM_TYPE: u64 = 15;
const EXE_TYPE: u64 = 9;
const ROLZ_TYPE: u64 = 11;
const BWT_TYPE: u64 = 1;
const RANK_TYPE: u64 = 8;
const ZRLT_TYPE: u64 = 6;
const NONE_ENTROPY: u64 = 0;
const HUFFMAN_ENTROPY: u64 = 1;
const ANS0_ENTROPY: u64 = 5;
const FPAQ_ENTROPY: u64 = 2;
const CM_ENTROPY: u64 = 6;
const TPAQ_ENTROPY: u64 = 7;
const TPAQX_ENTROPY: u64 = 9;
const SRT_TYPE: u64 = 13;
const LZP_TYPE: u64 = 14;
const RLT_TYPE: u64 = 5;
const SMALL_BLOCK_SIZE: usize = 15;
const BFF_ONE_SHIFT: u64 = 6;
const BFF_MAX_SHIFT: u64 = 42; // (8-1)*6

/// Per-stream block-content checksum (the real CLI's -x/-x32/-x64). `None`
/// means ckSize=0 (the default, and the only mode this project supported
/// before checksums were added).
pub enum Hasher {
    None,
    H32(XxHash32),
    H64(XxHash64),
}

impl Hasher {
    /// `ck_size`: 0 = none, 1 = 32-bit (XXHash32), 2 = 64-bit (XXHash64) --
    /// the same encoding as the stream header's ckSize field and the real
    /// CLI's `checksum` context value (32/64) once normalized to Go's
    /// internal ckSize units.
    pub fn new(ck_size: u64) -> Result<Self, String> {
        match ck_size {
            0 => Ok(Hasher::None),
            1 => Ok(Hasher::H32(XxHash32::new(BITSTREAM_TYPE as u32))),
            2 => Ok(Hasher::H64(XxHash64::new(BITSTREAM_TYPE))),
            other => Err(format!("Invalid checksum size: {}", other)),
        }
    }

    fn ck_size(&self) -> u64 {
        match self {
            Hasher::None => 0,
            Hasher::H32(_) => 1,
            Hasher::H64(_) => 2,
        }
    }

    /// The checksum to embed for one block's original (pre-transform)
    /// bytes, as (value, byte-width): `None` when no checksum is enabled.
    fn checksum(&self, data: &[u8]) -> Option<(u64, u8)> {
        match self {
            Hasher::None => None,
            Hasher::H32(h) => Some((h.hash(data) as u64, 4)),
            Hasher::H64(h) => Some((h.hash(data), 8)),
        }
    }
}

fn write_stream_header(bw: &mut BitWriter, entropy_type: u64, transform_type: u64, block_size: u32, hasher: &Hasher) {
    let ck_size = hasher.ck_size();
    bw.write_bits(BITSTREAM_TYPE, 32);
    bw.write_bits(BITSTREAM_FORMAT_VERSION, 4);
    bw.write_bits(ck_size, 2);
    bw.write_bits(entropy_type, 5);
    bw.write_bits(transform_type, 48);
    bw.write_bits((block_size as u64) >> 4, 28);
    bw.write_bits(0, 2); // szMask = 0 (input size not provided)
    bw.write_bits(0, 15); // padding

    let seed = 0x01030507u32.wrapping_mul(BITSTREAM_FORMAT_VERSION as u32);
    let mut cksum = HASH.wrapping_mul(seed);
    cksum = mix32(cksum, HASH, ck_size as u32);
    cksum = mix32(cksum, HASH, entropy_type as u32);
    cksum = mix32(cksum, HASH, (transform_type >> 32) as u32);
    cksum = mix32(cksum, HASH, transform_type as u32);
    cksum = mix32(cksum, HASH, block_size);
    cksum = (cksum >> 23) ^ (cksum >> 3);
    bw.write_bits(cksum as u64, 24);
}

/// Writes one already-built per-block byte buffer into the shared stream,
/// preceded by its variable-width length prefix (5 bits + lw bits).
/// `written` must be the block's EXACT bit length, not the byte-rounded-up
/// `encoded_block.len()*8` -- those differ whenever the block ends with a
/// bit-packed (non-byte-aligned) entropy payload, e.g. Huffman at level 2,
/// and the block's own header checksum is computed over the exact value.
fn write_framed_block(bw: &mut BitWriter, encoded_block: &[u8], written: u64) {
    let lw: u32 = if written < 8 {
        3
    } else {
        log2_no_check(((written >> 3) as u32).max(1)) + 4
    };
    bw.write_bits((lw - 3) as u64, 5);
    bw.write_bits(written, lw);
    bw.write_array(encoded_block, written as usize);
}

#[inline]
fn mix32(checksum: u32, hash: u32, value: u32) -> u32 {
    let c = checksum ^ (hash.wrapping_mul(!value));
    let c = c.rotate_left(13);
    c.wrapping_mul(5).wrapping_add(0x52DCE729)
}

fn log2_bytes_needed(x: u32) -> u32 {
    // matches internal.Log2NoCheck: floor(log2(x))>>3 + 1, with x<256 -> 1
    if x < 256 {
        1
    } else {
        (31 - x.leading_zeros()) / 8 + 1
    }
}

/// Splits `data` into `block_size`-byte chunks (the last one may be
/// shorter) and encodes them concurrently across up to
/// `available_parallelism()` OS threads, returning each block's
/// `(encoded_bytes, written_bits)` in original order -- ready to feed
/// straight into `write_framed_block` in a plain sequential loop.
///
/// Every block in this container format is fully self-contained: each
/// `encode_blockN` call gets a fresh entropy-coder instance (TPAQ/CM/FPAQ
/// all reset per block) and every transform either has no cross-block
/// state at all or explicitly reinitializes it at the top of `forward()`
/// (e.g. LzpCodec/LzxCodec's hash tables) -- exactly the property the
/// real CLI's own `-j` concurrency already relies on for the identical
/// wire format. So encoding blocks out of order is always safe; only the
/// *output* order must be preserved, which this does by writing each
/// result back into its original slot before the caller frames it.
///
/// `make_state` builds one thread-local scratch value per worker, created
/// once and reused across every block that worker handles -- e.g. a
/// `Bwt`'s internal buffers get reallocated only on growth, not per
/// block, matching the reuse pattern the single-threaded loop used to get
/// from sharing one instance across the whole call. `encode_one` encodes
/// a single block given that worker's state.
fn encode_blocks_parallel<S, F>(data: &[u8], block_size: u32, make_state: impl Fn() -> S + Sync, encode_one: F) -> Vec<(Vec<u8>, u64)>
where
    S: Send,
    F: Fn(&mut S, &[u8]) -> (Vec<u8>, u64) + Sync,
{
    let mut ranges = Vec::new();
    let mut offset = 0usize;

    while offset < data.len() {
        let len = (block_size as usize).min(data.len() - offset);
        ranges.push((offset, len));
        offset += len;
    }

    if ranges.is_empty() {
        return Vec::new();
    }

    let workers = worker_count(ranges.len());
    let mut results: Vec<Option<(Vec<u8>, u64)>> = (0..ranges.len()).map(|_| None).collect();

    if workers <= 1 {
        let mut state = make_state();

        for (i, &(off, len)) in ranges.iter().enumerate() {
            results[i] = Some(encode_one(&mut state, &data[off..off + len]));
        }
    } else {
        std::thread::scope(|scope| {
            let chunk = (ranges.len() + workers - 1) / workers;
            let mut handles = Vec::new();

            for start in (0..ranges.len()).step_by(chunk) {
                let end = (start + chunk).min(ranges.len());
                let ranges_ref = &ranges;
                let make_state_ref = &make_state;
                let encode_one_ref = &encode_one;

                handles.push(scope.spawn(move || {
                    let mut state = make_state_ref();
                    let mut local = Vec::with_capacity(end - start);

                    for i in start..end {
                        let (off, len) = ranges_ref[i];
                        local.push((i, encode_one_ref(&mut state, &data[off..off + len])));
                    }

                    local
                }));
            }

            for h in handles {
                for (i, res) in h.join().expect("encoder worker thread panicked") {
                    results[i] = Some(res);
                }
            }
        });
    }

    results.into_iter().map(|r| r.expect("every block range was assigned to exactly one worker")).collect()
}

/// Encodes `data` as a complete level-1 (LZX & NONE) kanzi bitstream:
/// no input-size hint, single job, block size `block_size`. `ck_size`
/// selects the optional per-block checksum (0=none, 1=32-bit, 2=64-bit --
/// the real CLI's -x32/-x64).
pub fn encode_level1(data: &[u8], block_size: u32, ck_size: u64) -> Vec<u8> {
    let hasher = Hasher::new(ck_size).expect("invalid ck_size");
    let mut bw = BitWriter::new();
    let transform_type: u64 = LZX_TYPE << BFF_MAX_SHIFT;
    write_stream_header(&mut bw, NONE_ENTROPY, transform_type, block_size, &hasher);

    let blocks = encode_blocks_parallel(data, block_size, || LzxCodec::new(true), |lzx, block| {
        encode_block(block, lzx, hasher.checksum(block))
    });

    for (encoded_block, written) in &blocks {
        write_framed_block(&mut bw, encoded_block, *written);
    }

    // End marker: an empty (0-bit) block signals end of stream.
    bw.write_bits(0, 5);
    bw.write_bits(0, 3);

    bw.finish()
}

/// Encodes `data` as a complete level-2 (DNA+LZ & HUFFMAN) kanzi bitstream.
/// The DNA/Alias stage is fully ported (see alias.rs), including the genuine
/// nucleotide-input success path.
pub fn encode_level2(data: &[u8], block_size: u32, ck_size: u64) -> Vec<u8> {
    let hasher = Hasher::new(ck_size).expect("invalid ck_size");
    let mut bw = BitWriter::new();
    let transform_type: u64 =
        (DNA_TYPE << BFF_MAX_SHIFT) | (LZ_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT));
    write_stream_header(&mut bw, HUFFMAN_ENTROPY, transform_type, block_size, &hasher);

    let blocks = encode_blocks_parallel(data, block_size, || LzxCodec::new(false), |lzx, block| {
        encode_block2(block, lzx, hasher.checksum(block))
    });

    for (encoded_block, written) in &blocks {
        write_framed_block(&mut bw, encoded_block, *written);
    }

    bw.write_bits(0, 5);
    bw.write_bits(0, 3);

    bw.finish()
}

/// DNA/Alias stage (transform.AliasCodec, onlyDNA=true). Returns the
/// post-stage bytes, its skip bit (0 = applied, 1 = declined), and the
/// detected data type -- which LZCodec.Forward consults (ctx["dataType"] is
/// set as a side effect even when AliasCodec itself declines).
fn dna_stage(block: &[u8]) -> (Vec<u8>, u8, DataType) {
    let mut buf = vec![0u8; alias::max_encoded_len(block.len())];

    match alias::forward(block, &mut buf, DataType::Undefined, true) {
        Ok((_, n, dt)) => {
            buf.truncate(n);
            (buf, 0, dt)
        }
        Err((_, dt)) => (block.to_vec(), 1, dt),
    }
}

fn encode_block2(data: &[u8], lzx: &mut LzxCodec, checksum: Option<(u64, u8)>) -> (Vec<u8>, u64) {
    let block_len = data.len();

    if block_len <= SMALL_BLOCK_SIZE {
        return finish_block(data.to_vec(), block_len, true, 0, checksum);
    }

    let (stage1_out, skip_bit0, dt) = dna_stage(data);

    let min_match = if dt == DataType::Dna {
        lzx::MIN_MATCH6
    } else {
        lzx::MIN_MATCH4
    };
    let (stage2_out, skip_bit1): (Vec<u8>, u8) = if dt == DataType::SmallAlphabet {
        (stage1_out, 1)
    } else {
        let mut dst = vec![0u8; LzxCodec::max_encoded_len(stage1_out.len())];

        match lzx.forward(&stage1_out, &mut dst, min_match) {
            Ok((_, n)) => {
                dst.truncate(n);
                (dst, 0)
            }
            Err(_) => (stage1_out, 1),
        }
    };

    let skip_flags: u8 = (skip_bit0 << 7) | (skip_bit1 << 6) | 0x3F;
    let skip_nibble = skip_flags >> 4;

    // Huffman-entropy-encode the (post-transform) payload.
    let mut henc = HuffmanEncoder::new();
    let mut ebw = BitWriter::new();
    henc.write(&stage2_out, &mut ebw);
    let (entropy_bytes, entropy_bit_len) = ebw.finish_with_len();

    let normal = finish_block2(
        entropy_bytes,
        entropy_bit_len,
        stage2_out.len(),
        false,
        skip_nibble,
        checksum,
    );
    maybe_transformed_copy(normal, &stage2_out, skip_flags, 2, checksum)
}

/// Assembles [mode][preTransformLength][headerChecksum][entropy payload]
/// for a non-copy L2 block, where the payload is already entropy-coded bits
/// (possibly not byte-aligned in length).
fn finish_block2(
    entropy_bytes: Vec<u8>,
    entropy_bit_len: u64,
    post_transform_len: usize,
    is_copy: bool,
    skip_nibble: u8,
    payload_checksum: Option<(u64, u8)>,
) -> (Vec<u8>, u64) {
    let data_size = log2_bytes_needed(post_transform_len as u32);
    let mut mode: u8 = (((data_size - 1) & 0x03) as u8) << 5;

    if is_copy {
        mode |= 0x80;
    }

    mode |= skip_nibble;

    let header_skip_flags: u8 = if is_copy { 0 } else { (mode << 4) | 0x0F };

    let mut prefix = Vec::with_capacity(2 + data_size as usize);
    prefix.push(mode);

    for i in (0..data_size).rev() {
        prefix.push(((post_transform_len as u64 >> (8 * i)) & 0xFF) as u8);
    }

    let checksum_bits = payload_checksum.map(|(_, w)| w as u64 * 8).unwrap_or(0);
    let written = (prefix.len() as u64 + 1) * 8 + checksum_bits + entropy_bit_len; // +1 for the header checksum byte

    let mut cksum = HASH.wrapping_mul(0x01030507u32);
    cksum = mix32(cksum, HASH, mode as u32);
    cksum = mix32(cksum, HASH, header_skip_flags as u32);
    cksum = mix32(cksum, HASH, post_transform_len as u32);
    cksum = mix32(cksum, HASH, (written >> 32) as u32);
    cksum = mix32(cksum, HASH, written as u32);
    cksum = (cksum >> 23) ^ (cksum >> 3);

    if std::env::var("KDEBUG").is_ok() {
        eprintln!(
            "DEBUG mode={:#04x} data_size={} post_transform_len={} header_skip_flags={:#04x} written={} entropy_bit_len={} cksum_byte={:#04x}",
            mode, data_size, post_transform_len, header_skip_flags, written, entropy_bit_len, cksum as u8
        );
    }

    let mut bw = BitWriter::new();
    bw.write_array(&prefix, prefix.len() * 8);
    bw.write_bits(cksum as u64, 8);

    if let Some((value, width)) = payload_checksum {
        bw.write_bits(value, width as u32 * 8);
    }

    bw.write_array(&entropy_bytes, entropy_bit_len as usize);
    (bw.finish(), written)
}

/// Small-block (<=15 bytes) copy path, shared shape with level 1's
/// finish_block but kept local to encode_block2 for now.
/// `payload_checksum` is the optional block-content checksum (computed on
/// the original, pre-transform bytes -- see `Hasher::checksum`), written
/// right after the header checksum byte and before the payload, exactly
/// like Go's "Write checksum" step. It contributes to `written` (and thus
/// to the header checksum, which covers `written`) like everything else.
fn finish_block(
    payload: Vec<u8>,
    post_transform_len: usize,
    is_copy: bool,
    skip_nibble_override: u8,
    payload_checksum: Option<(u64, u8)>,
) -> (Vec<u8>, u64) {
    let data_size = log2_bytes_needed(post_transform_len as u32);
    let mut mode: u8 = (((data_size - 1) & 0x03) as u8) << 5;

    if is_copy {
        mode |= 0x80;
    }

    mode |= if is_copy { 0x07 } else { skip_nibble_override };

    let header_skip_flags: u8 = if is_copy { 0 } else { (mode << 4) | 0x0F };

    let mut buf = Vec::with_capacity(2 + data_size as usize + payload.len());
    buf.push(mode);

    for i in (0..data_size).rev() {
        buf.push(((post_transform_len as u64 >> (8 * i)) & 0xFF) as u8);
    }

    let checksum_index = buf.len();
    buf.push(0);

    if let Some((value, width)) = payload_checksum {
        buf.extend_from_slice(&value.to_be_bytes()[8 - width as usize..]);
    }

    buf.extend_from_slice(&payload);

    let written = buf.len() as u64 * 8;
    let mut cksum = HASH.wrapping_mul(0x01030507u32);
    cksum = mix32(cksum, HASH, mode as u32);
    cksum = mix32(cksum, HASH, header_skip_flags as u32);
    cksum = mix32(cksum, HASH, post_transform_len as u32);
    cksum = mix32(cksum, HASH, (written >> 32) as u32);
    cksum = mix32(cksum, HASH, written as u32);
    cksum = (cksum >> 23) ^ (cksum >> 3);
    buf[checksum_index] = cksum as u8;

    (buf, written)
}

/// Assembles a block's local buffer for an arbitrary transform-sequence
/// length, choosing the short (skip nibble packed into mode, len<=4) or
/// long (separate skipFlags byte, len>4) header form exactly as
/// encodingTask.encode does. `payload` is (entropy_bytes, exact_bit_len).
fn finish_block_multi(
    payload: (Vec<u8>, u64),
    post_transform_len: usize,
    is_copy: bool,
    skip_flags: u8,
    num_transforms: usize,
    payload_checksum: Option<(u64, u8)>,
) -> (Vec<u8>, u64) {
    let (entropy_bytes, entropy_bit_len) = payload;
    let data_size = log2_bytes_needed(post_transform_len as u32);
    let mut mode: u8 = (((data_size - 1) & 0x03) as u8) << 5;

    if is_copy {
        mode |= 0x80;
    }

    let long_form = !is_copy && num_transforms > 4;
    let header_skip_flags: u8;
    let mut prefix = Vec::new();

    if long_form {
        mode |= 0x10; // _TRANSFORMS_MASK
        header_skip_flags = skip_flags;
        prefix.push(mode);
        prefix.push(skip_flags);
    } else {
        mode |= skip_flags >> 4;
        header_skip_flags = if is_copy { 0 } else { (mode << 4) | 0x0F };
        prefix.push(mode);
    }

    for i in (0..data_size).rev() {
        prefix.push(((post_transform_len as u64 >> (8 * i)) & 0xFF) as u8);
    }

    let checksum_bits = payload_checksum.map(|(_, w)| w as u64 * 8).unwrap_or(0);
    let written = (prefix.len() as u64 + 1) * 8 + checksum_bits + entropy_bit_len; // +1 for the header checksum byte

    let mut cksum = HASH.wrapping_mul(0x01030507u32);
    cksum = mix32(cksum, HASH, mode as u32);
    cksum = mix32(cksum, HASH, header_skip_flags as u32);
    cksum = mix32(cksum, HASH, post_transform_len as u32);
    cksum = mix32(cksum, HASH, (written >> 32) as u32);
    cksum = mix32(cksum, HASH, written as u32);
    cksum = (cksum >> 23) ^ (cksum >> 3);

    let mut bw = BitWriter::new();
    bw.write_array(&prefix, prefix.len() * 8);
    bw.write_bits(cksum as u64, 8);

    if let Some((value, width)) = payload_checksum {
        bw.write_bits(value, width as u32 * 8);
    }

    bw.write_array(&entropy_bytes, entropy_bit_len as usize);
    (bw.finish(), written)
}

/// Assembles a block in transformed-copy form: the entropy stage is bypassed
/// and the raw post-transform bytes are stored. Mirrors the copyStream
/// re-emit path of Go's encodingTask.encode: copyMode = mode | COPY |
/// TRANSFORMS_MASK, with a separate skipFlags byte for long (>4) sequences.
fn finish_block_transformed_copy(
    payload: &[u8],
    skip_flags: u8,
    num_transforms: usize,
    payload_checksum: Option<(u64, u8)>,
) -> (Vec<u8>, u64) {
    let post_len = payload.len();
    let data_size = log2_bytes_needed(post_len as u32);
    let mut mode: u8 = (((data_size - 1) & 0x03) as u8) << 5;
    let header_skip_flags: u8;
    let mut prefix = Vec::new();

    if num_transforms > 4 {
        mode |= 0x10 | 0x80; // _TRANSFORMS_MASK (long form, kept) + copyMode
        header_skip_flags = skip_flags;
        prefix.push(mode);
        prefix.push(skip_flags);
    } else {
        mode |= skip_flags >> 4;
        mode |= 0x80 | 0x10; // copyMode
        header_skip_flags = (mode << 4) | 0x0F;
        prefix.push(mode);
    }

    for i in (0..data_size).rev() {
        prefix.push(((post_len as u64 >> (8 * i)) & 0xFF) as u8);
    }

    let mut buf = prefix;
    let checksum_index = buf.len();
    buf.push(0); // placeholder, patched below

    if let Some((value, width)) = payload_checksum {
        buf.extend_from_slice(&value.to_be_bytes()[8 - width as usize..]);
    }

    buf.extend_from_slice(payload);

    let written = buf.len() as u64 * 8;
    let mut cksum = HASH.wrapping_mul(0x01030507u32);
    cksum = mix32(cksum, HASH, mode as u32);
    cksum = mix32(cksum, HASH, header_skip_flags as u32);
    cksum = mix32(cksum, HASH, post_len as u32);
    cksum = mix32(cksum, HASH, (written >> 32) as u32);
    cksum = mix32(cksum, HASH, written as u32);
    cksum = (cksum >> 23) ^ (cksum >> 3);
    buf[checksum_index] = cksum as u8;

    (buf, written)
}

/// Go's encodingTask re-emits a block in transformed-copy form whenever the
/// entropy-coded block (header included) is bigger than the raw
/// post-transform bytes (strict `<`, see CompressedStream.go). With a
/// copy entropy (NONE) this always triggers; with Huffman it triggers only
/// when entropy coding expands the data. `normal` is the already-assembled
/// regular block, `post_payload` the raw post-transform bytes it encodes.
fn maybe_transformed_copy(
    normal: (Vec<u8>, u64),
    post_payload: &[u8],
    skip_flags: u8,
    num_transforms: usize,
    payload_checksum: Option<(u64, u8)>,
) -> (Vec<u8>, u64) {
    let (bytes, written) = normal;

    if (post_payload.len() as u64) < (written + 7) >> 3 {
        finish_block_transformed_copy(post_payload, skip_flags, num_transforms, payload_checksum)
    } else {
        (bytes, written)
    }
}

/// Encodes `data` as a complete level-3 (TEXT+UTF+PACK+MM+LZX & HUFFMAN)
/// kanzi bitstream. TEXT, UTF, PACK, MM/FSD and LZX are all fully ported.
pub fn encode_level3(data: &[u8], block_size: u32, ck_size: u64) -> Vec<u8> {
    let hasher = Hasher::new(ck_size).expect("invalid ck_size");
    let mut bw = BitWriter::new();
    let transform_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (PACK_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (MM_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (LZX_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    write_stream_header(&mut bw, HUFFMAN_ENTROPY, transform_type, block_size, &hasher);

    let blocks = encode_blocks_parallel(data, block_size, || LzxCodec::new(true), |lzx, block| {
        encode_block3(block, lzx, block_size, hasher.checksum(block))
    });

    for (encoded_block, written) in &blocks {
        write_framed_block(&mut bw, encoded_block, *written);
    }

    bw.write_bits(0, 5);
    bw.write_bits(0, 3);

    bw.finish()
}

fn encode_block3(data: &[u8], lzx: &mut LzxCodec, block_size: u32, checksum: Option<(u64, u8)>) -> (Vec<u8>, u64) {
    let block_len = data.len();

    if block_len <= SMALL_BLOCK_SIZE {
        return finish_block(data.to_vec(), block_len, true, 0, checksum);
    }

    // Stage 1: TEXT (real implementation, see text_codec.rs). The data type
    // accompanies both outcomes (Go's ctx["dataType"] side effect) and feeds
    // the PACK stage below.
    let mut text_dst = vec![0u8; text_codec::max_encoded_len(block_len)];
    let (stage1_out, skip_text, dt_text): (Vec<u8>, u8, DataType) =
        match text_codec::forward(data, &mut text_dst, block_size, false) {
            Ok((_, n, dt)) => {
                text_dst.truncate(n);
                (text_dst, 0, dt)
            }
            Err((_, dt)) => (data.to_vec(), 1, dt),
        };

    // Stage 2: UTF (real implementation, see utf.rs). The data type threads
    // through to PACK below (Go's ctx["dataType"]).
    let mut utf_dst = vec![0u8; utf::max_encoded_len(stage1_out.len())];
    let (stage2_out, skip_utf, dt_utf): (Vec<u8>, u8, DataType) =
        match utf::forward(&stage1_out, &mut utf_dst, dt_text) {
            Ok((_, n, dt)) => {
                utf_dst.truncate(n);
                (utf_dst, 0, dt)
            }
            Err((_, dt)) => (stage1_out.clone(), 1, dt),
        };

    // Stage 3: PACK (real implementation, see alias.rs; onlyDNA=false).
    // The data type threads through to MM below (Go's ctx["dataType"]).
    let mut pack_dst = vec![0u8; alias::max_encoded_len(stage2_out.len())];
    let (stage3_out, skip_pack, dt_pack): (Vec<u8>, u8, DataType) =
        match alias::forward(&stage2_out, &mut pack_dst, dt_utf, false) {
            Ok((_, n, dt)) => {
                pack_dst.truncate(n);
                (pack_dst, 0, dt)
            }
            Err((_, dt)) => (stage2_out.clone(), 1, dt),
        };

    // Stage 4: MM/FSD (real implementation, see fsd.rs)
    let mut mm_dst = vec![0u8; fsd::max_encoded_len(stage3_out.len())];
    let (stage4_out, skip_mm): (Vec<u8>, u8) = match fsd::forward(&stage3_out, &mut mm_dst, dt_pack)
    {
        Ok((_, n, _)) => {
            mm_dst.truncate(n);
            (mm_dst, 0)
        }
        Err(_) => (stage3_out.clone(), 1),
    };

    // Stage 5: LZX (real implementation, extra=true, reused from level 1)
    let mut lzx_dst = vec![0u8; LzxCodec::max_encoded_len(stage4_out.len())];
    let (stage5_out, skip_lzx): (Vec<u8>, u8) =
        match lzx.forward(&stage4_out, &mut lzx_dst, lzx::MIN_MATCH4) {
            Ok((_, n)) => {
                lzx_dst.truncate(n);
                (lzx_dst, 0)
            }
            Err(_) => (stage4_out.clone(), 1),
        };

    let skip_flags: u8 = (skip_text << 7)
        | (skip_utf << 6)
        | (skip_pack << 5)
        | (skip_mm << 4)
        | (skip_lzx << 3)
        | 0x07;

    let mut henc = HuffmanEncoder::new();
    let mut ebw = BitWriter::new();
    henc.write(&stage5_out, &mut ebw);
    let entropy_payload = ebw.finish_with_len();

    let normal = finish_block_multi(entropy_payload, stage5_out.len(), false, skip_flags, 5, checksum);
    maybe_transformed_copy(normal, &stage5_out, skip_flags, 5, checksum)
}

/// Encodes `data` as a complete level-4
/// (TEXT+UTF+EXE+PACK+MM+ROLZ & NONE) kanzi bitstream. TEXT, UTF, EXE, PACK,
/// MM/FSD and ROLZ are all fully ported.
pub fn encode_level4(data: &[u8], block_size: u32, ck_size: u64) -> Vec<u8> {
    let mut bw = BitWriter::new();
    let transform_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (EXE_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (PACK_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (MM_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT))
        | (ROLZ_TYPE << (BFF_MAX_SHIFT - 5 * BFF_ONE_SHIFT));
    let hasher = Hasher::new(ck_size).expect("invalid ck_size");
    write_stream_header(&mut bw, NONE_ENTROPY, transform_type, block_size, &hasher);

    let blocks = encode_blocks_parallel(data, block_size, RolzCodec::new, |rolz, block| {
        encode_block4(block, rolz, block_size, hasher.checksum(block))
    });

    for (encoded_block, written) in &blocks {
        write_framed_block(&mut bw, encoded_block, *written);
    }

    bw.write_bits(0, 5);
    bw.write_bits(0, 3);

    bw.finish()
}

fn encode_block4(data: &[u8], rolz: &mut RolzCodec, block_size: u32, checksum: Option<(u64, u8)>) -> (Vec<u8>, u64) {
    let block_len = data.len();

    if block_len <= SMALL_BLOCK_SIZE {
        return finish_block(data.to_vec(), block_len, true, 0, checksum);
    }

    // Stage 1: TEXT (real implementation, see text_codec.rs)
    let mut text_dst = vec![0u8; text_codec::max_encoded_len(block_len)];
    let (stage1_out, skip_text, dt_text): (Vec<u8>, u8, DataType) =
        match text_codec::forward(data, &mut text_dst, block_size, false) {
            Ok((_, n, dt)) => {
                text_dst.truncate(n);
                (text_dst, 0, dt)
            }
            Err((_, dt)) => (data.to_vec(), 1, dt),
        };

    // Stage 2: UTF (real implementation, see utf.rs). The data type threads
    // through to PACK below (Go's ctx["dataType"]).
    let mut utf_dst = vec![0u8; utf::max_encoded_len(stage1_out.len())];
    let (stage2_out, skip_utf, dt_utf): (Vec<u8>, u8, DataType) =
        match utf::forward(&stage1_out, &mut utf_dst, dt_text) {
            Ok((_, n, dt)) => {
                utf_dst.truncate(n);
                (utf_dst, 0, dt)
            }
            Err((_, dt)) => (stage1_out.clone(), 1, dt),
        };

    // Stage 3: EXE (real implementation, see exe.rs). The data type threads
    // through to PACK below (Go's ctx["dataType"]).
    let mut exe_dst = vec![0u8; exe::max_encoded_len(stage2_out.len())];
    let (stage3_out, skip_exe, dt_exe): (Vec<u8>, u8, DataType) =
        match exe::forward(&stage2_out, &mut exe_dst, dt_utf) {
            Ok((_, n, dt)) => {
                exe_dst.truncate(n);
                (exe_dst, 0, dt)
            }
            Err((_, dt)) => (stage2_out.clone(), 1, dt),
        };

    // Stage 4: PACK (real implementation, see alias.rs; onlyDNA=false)
    let mut pack_dst = vec![0u8; alias::max_encoded_len(stage3_out.len())];
    let (stage4_out, skip_pack, dt_pack): (Vec<u8>, u8, DataType) =
        match alias::forward(&stage3_out, &mut pack_dst, dt_exe, false) {
            Ok((_, n, dt)) => {
                pack_dst.truncate(n);
                (pack_dst, 0, dt)
            }
            Err((_, dt)) => (stage3_out.clone(), 1, dt),
        };

    // Stage 5: MM/FSD (real implementation, see fsd.rs). The data type
    // threads through to ROLZ below (Go's ctx["dataType"]).
    let mut mm_dst = vec![0u8; fsd::max_encoded_len(stage4_out.len())];
    let (stage5_out, skip_mm, dt_mm): (Vec<u8>, u8, DataType) =
        match fsd::forward(&stage4_out, &mut mm_dst, dt_pack) {
            Ok((_, n, dt)) => {
                mm_dst.truncate(n);
                (mm_dst, 0, dt)
            }
            Err((_, dt)) => (stage4_out.clone(), 1, dt),
        };

    // Stage 6: ROLZ (real implementation, see rolz.rs)
    let mut rolz_dst = vec![0u8; crate::rolz::max_encoded_len(stage5_out.len())];
    let (stage6_out, skip_rolz): (Vec<u8>, u8) =
        match rolz.forward(&stage5_out, &mut rolz_dst, dt_mm) {
            Ok((_, n, _)) => {
                rolz_dst.truncate(n);
                (rolz_dst, 0)
            }
            Err(_) => (stage5_out.clone(), 1),
        };

    // 6-transform sequence: top 6 skip bits used, low 2 stay set (Go's
    // _TRANSFORM_SKIP_MASK = 0xFF with one bit cleared per applied stage).
    let skip_flags: u8 = (skip_text << 7)
        | (skip_utf << 6)
        | (skip_exe << 5)
        | (skip_pack << 4)
        | (skip_mm << 3)
        | (skip_rolz << 2)
        | 0x03;

    // NONE entropy: the payload is the raw post-transform bytes. The normal
    // form's header alone already makes it bigger than the raw payload, so
    // Go's strict < rule always selects transformed-copy here -- build it
    // directly (post_len > 0 always: blocks are non-empty).
    finish_block_transformed_copy(&stage6_out, skip_flags, 6, checksum)
}

/// Encodes `data` as a complete level-5 (TEXT+UTF+BWT+RANK+ZRLT & ANS0)
/// kanzi bitstream. All stages are fully ported.
pub fn encode_level5(data: &[u8], block_size: u32, ck_size: u64) -> Vec<u8> {
    let mut bw = BitWriter::new();
    let transform_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (BWT_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (RANK_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (ZRLT_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    let hasher = Hasher::new(ck_size).expect("invalid ck_size");
    write_stream_header(&mut bw, ANS0_ENTROPY, transform_type, block_size, &hasher);

    let blocks = encode_blocks_parallel(data, block_size, Bwt::new, |bwt, block| {
        encode_block5(block, bwt, block_size, hasher.checksum(block))
    });

    for (encoded_block, written) in &blocks {
        write_framed_block(&mut bw, encoded_block, *written);
    }

    bw.write_bits(0, 5);
    bw.write_bits(0, 3);

    bw.finish()
}

fn encode_block5(data: &[u8], bwt: &mut Bwt, block_size: u32, checksum: Option<(u64, u8)>) -> (Vec<u8>, u64) {
    let block_len = data.len();

    if block_len <= SMALL_BLOCK_SIZE {
        return finish_block(data.to_vec(), block_len, true, 0, checksum);
    }

    // Stage 1: TEXT (codec 2 covers ANS0, like HUFFMAN/NONE; see Factory.go).
    // The data type accompanies both outcomes (Go's ctx["dataType"]) and
    // feeds the UTF stage below.
    let mut text_dst = vec![0u8; text_codec::max_encoded_len(block_len)];
    let (stage1_out, skip_text, dt_text): (Vec<u8>, u8, DataType) =
        match text_codec::forward(data, &mut text_dst, block_size, false) {
            Ok((_, n, dt)) => {
                text_dst.truncate(n);
                (text_dst, 0, dt)
            }
            Err((_, dt)) => (data.to_vec(), 1, dt),
        };

    // Stage 2: UTF (real implementation, see utf.rs)
    let mut utf_dst = vec![0u8; utf::max_encoded_len(stage1_out.len())];
    let (stage2_out, skip_utf): (Vec<u8>, u8) =
        match utf::forward(&stage1_out, &mut utf_dst, dt_text) {
            Ok((_, n, _)) => {
                utf_dst.truncate(n);
                (utf_dst, 0)
            }
            Err(_) => (stage1_out.clone(), 1),
        };

    // Stage 3: BWT (real implementation, see bwt.rs). Practically infallible
    // with correct sizing; an error degrades to decline like Go's Sequence.
    let mut bwt_dst = vec![0u8; crate::bwt::max_encoded_len(stage2_out.len())];
    let (stage3_out, skip_bwt): (Vec<u8>, u8) = match bwt.forward(&stage2_out, &mut bwt_dst) {
        Ok((_, n)) => {
            bwt_dst.truncate(n);
            (bwt_dst, 0)
        }
        Err(_) => (stage2_out.clone(), 1),
    };

    // Stage 4: RANK (real implementation, see sbrt.rs; total order, never
    // declines semantically).
    let rank = Sbrt::new_rank();
    let mut rank_dst = vec![0u8; Sbrt::max_encoded_len(stage3_out.len())];
    let (stage4_out, skip_rank): (Vec<u8>, u8) = match rank.forward(&stage3_out, &mut rank_dst) {
        Ok((_, n)) => {
            rank_dst.truncate(n);
            (rank_dst, 0)
        }
        Err(_) => (stage3_out.clone(), 1),
    };

    // Stage 5: ZRLT (real implementation, see zrlt.rs)
    let mut zrlt_dst = vec![0u8; zrlt::max_encoded_len(stage4_out.len())];
    let (stage5_out, skip_zrlt): (Vec<u8>, u8) = match zrlt::forward(&stage4_out, &mut zrlt_dst) {
        Ok((_, n)) => {
            zrlt_dst.truncate(n);
            (zrlt_dst, 0)
        }
        Err(_) => (stage4_out.clone(), 1),
    };

    // 5-transform sequence: top 5 skip bits used, low 3 stay set.
    let skip_flags: u8 = (skip_text << 7)
        | (skip_utf << 6)
        | (skip_bwt << 5)
        | (skip_rank << 4)
        | (skip_zrlt << 3)
        | 0x07;

    // ANS0 entropy (order 0, default chunk/logRange -- like ROLZ's litEnc).
    let mut aenc = AnsEncoder::new(0, None, None).expect("ans encoder params");
    let mut ebw = BitWriter::new();
    aenc.write(&stage5_out, &mut ebw);
    let entropy_payload = ebw.finish_with_len();

    let normal = finish_block_multi(entropy_payload, stage5_out.len(), false, skip_flags, 5, checksum);
    maybe_transformed_copy(normal, &stage5_out, skip_flags, 5, checksum)
}

/// Encodes `data` as a complete level-6 (TEXT+UTF+BWT+SRT+ZRLT & FPAQ)
/// kanzi bitstream. All stages are fully ported.
pub fn encode_level6(data: &[u8], block_size: u32, ck_size: u64) -> Vec<u8> {
    let mut bw = BitWriter::new();
    let transform_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (BWT_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (SRT_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (ZRLT_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    let hasher = Hasher::new(ck_size).expect("invalid ck_size");
    write_stream_header(&mut bw, FPAQ_ENTROPY, transform_type, block_size, &hasher);

    let blocks = encode_blocks_parallel(data, block_size, Bwt::new, |bwt, block| {
        encode_block6(block, bwt, block_size, hasher.checksum(block))
    });

    for (encoded_block, written) in &blocks {
        write_framed_block(&mut bw, encoded_block, *written);
    }

    bw.write_bits(0, 5);
    bw.write_bits(0, 3);

    bw.finish()
}

fn encode_block6(data: &[u8], bwt: &mut Bwt, block_size: u32, checksum: Option<(u64, u8)>) -> (Vec<u8>, u64) {
    let block_len = data.len();

    if block_len <= SMALL_BLOCK_SIZE {
        return finish_block(data.to_vec(), block_len, true, 0, checksum);
    }

    // Stage 1: TEXT (codec 1 for FPAQ/CM/TPAQ; see Factory.go newToken).
    let mut text_dst = vec![0u8; text_codec::max_encoded_len(block_len)];
    let (stage1_out, skip_text, dt_text): (Vec<u8>, u8, DataType) =
        match text_codec1::forward(data, &mut text_dst, block_size, false) {
            Ok((_, n, dt)) => {
                text_dst.truncate(n);
                (text_dst, 0, dt)
            }
            Err((_, dt)) => (data.to_vec(), 1, dt),
        };

    // Stage 2: UTF (real implementation, see utf.rs)
    let mut utf_dst = vec![0u8; utf::max_encoded_len(stage1_out.len())];
    let (stage2_out, skip_utf): (Vec<u8>, u8) =
        match utf::forward(&stage1_out, &mut utf_dst, dt_text) {
            Ok((_, n, _)) => {
                utf_dst.truncate(n);
                (utf_dst, 0)
            }
            Err(_) => (stage1_out.clone(), 1),
        };

    // Stage 3: BWT (real implementation, see bwt.rs)
    let mut bwt_dst = vec![0u8; crate::bwt::max_encoded_len(stage2_out.len())];
    let (stage3_out, skip_bwt): (Vec<u8>, u8) = match bwt.forward(&stage2_out, &mut bwt_dst) {
        Ok((_, n)) => {
            bwt_dst.truncate(n);
            (bwt_dst, 0)
        }
        Err(_) => (stage2_out.clone(), 1),
    };

    // Stage 4: SRT (real implementation, see srt.rs)
    let srt = Srt::new();
    let mut srt_dst = vec![0u8; Srt::max_encoded_len(stage3_out.len())];
    let (stage4_out, skip_srt): (Vec<u8>, u8) = match srt.forward(&stage3_out, &mut srt_dst) {
        Ok((_, n)) => {
            srt_dst.truncate(n);
            (srt_dst, 0)
        }
        Err(_) => (stage3_out.clone(), 1),
    };

    // Stage 5: ZRLT (real implementation, see zrlt.rs)
    let mut zrlt_dst = vec![0u8; zrlt::max_encoded_len(stage4_out.len())];
    let (stage5_out, skip_zrlt): (Vec<u8>, u8) = match zrlt::forward(&stage4_out, &mut zrlt_dst) {
        Ok((_, n)) => {
            zrlt_dst.truncate(n);
            (zrlt_dst, 0)
        }
        Err(_) => (stage4_out.clone(), 1),
    };

    // 5-transform sequence: top 5 skip bits used, low 3 stay set.
    let skip_flags: u8 = (skip_text << 7)
        | (skip_utf << 6)
        | (skip_bwt << 5)
        | (skip_srt << 4)
        | (skip_zrlt << 3)
        | 0x07;

    if std::env::var("STAGETRACE").is_ok() {
        eprintln!(
            "ENC6 lens: text={} bwt={} srt={} zrlt={}",
            stage1_out.len(),
            stage3_out.len(),
            stage4_out.len(),
            stage5_out.len()
        );
    }

    // FPAQ entropy (fresh instance per block, disposed at block end).
    let mut fenc = FpaqEncoder::new();
    let mut ebw = BitWriter::new();
    fenc.write(&stage5_out, &mut ebw);
    fenc.dispose(&mut ebw);
    let entropy_payload = ebw.finish_with_len();

    let normal = finish_block_multi(entropy_payload, stage5_out.len(), false, skip_flags, 5, checksum);
    maybe_transformed_copy(normal, &stage5_out, skip_flags, 5, checksum)
}

/// Encodes `data` as a complete level-0 (NONE&NONE, "store") kanzi
/// bitstream. transform_type is 0 (all 8 slots NONE); Go's Factory still
/// keeps one forced identity slot in the sequence (nbtr==0 -> nbtr=1), so
/// every block's skip_flags shows that one slot as "succeeded" (0x7F) --
/// there's nothing to skip since it's a pure no-op. Every block above
/// SMALL_BLOCK_SIZE ends up re-emitted as a "transformed copy" of the raw
/// bytes: NONE entropy's header-plus-payload total is never smaller than
/// the raw payload alone, so Go's strict `<` re-emit rule always fires
/// here (same reasoning as level 4's ROLZ-with-NONE-entropy path).
pub fn encode_level0(data: &[u8], block_size: u32, ck_size: u64) -> Vec<u8> {
    let hasher = Hasher::new(ck_size).expect("invalid ck_size");
    let mut bw = BitWriter::new();
    write_stream_header(&mut bw, NONE_ENTROPY, 0, block_size, &hasher);

    let blocks = encode_blocks_parallel(data, block_size, || (), |_state, block| {
        encode_block0(block, hasher.checksum(block))
    });

    for (encoded_block, written) in &blocks {
        write_framed_block(&mut bw, encoded_block, *written);
    }

    bw.write_bits(0, 5);
    bw.write_bits(0, 3);

    bw.finish()
}

fn encode_block0(data: &[u8], checksum: Option<(u64, u8)>) -> (Vec<u8>, u64) {
    let block_len = data.len();

    if block_len <= SMALL_BLOCK_SIZE {
        return finish_block(data.to_vec(), block_len, true, 0, checksum);
    }

    let skip_flags: u8 = 0x7F;
    let normal = finish_block_multi((data.to_vec(), (block_len as u64) * 8), block_len, false, skip_flags, 1, checksum);
    maybe_transformed_copy(normal, data, skip_flags, 1, checksum)
}

/// Encodes `data` as a complete level-7 (LZP+TEXT+UTF+BWT+LZP & CM)
/// kanzi bitstream. LZP appears twice in the sequence (slot0 and slot4),
/// each with its own fresh `LzpCodec` instance/hash table, matching Go's
/// "fresh transform instance per slot" behavior even when the same type
/// repeats.
pub fn encode_level7(data: &[u8], block_size: u32, ck_size: u64) -> Vec<u8> {
    let hasher = Hasher::new(ck_size).expect("invalid ck_size");
    let mut bw = BitWriter::new();
    let transform_type: u64 = (LZP_TYPE << BFF_MAX_SHIFT)
        | (TEXT_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (UTF_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (BWT_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (LZP_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    write_stream_header(&mut bw, CM_ENTROPY, transform_type, block_size, &hasher);

    let blocks = encode_blocks_parallel(
        data,
        block_size,
        || (LzpCodec::new(), Bwt::new(), LzpCodec::new()),
        |(lzp0, bwt, lzp1), block| encode_block7(block, lzp0, bwt, lzp1, block_size, hasher.checksum(block)),
    );

    for (encoded_block, written) in &blocks {
        write_framed_block(&mut bw, encoded_block, *written);
    }

    bw.write_bits(0, 5);
    bw.write_bits(0, 3);

    bw.finish()
}

fn encode_block7(
    data: &[u8],
    lzp0: &mut LzpCodec,
    bwt: &mut Bwt,
    lzp1: &mut LzpCodec,
    block_size: u32,
    checksum: Option<(u64, u8)>,
) -> (Vec<u8>, u64) {
    let block_len = data.len();

    if block_len <= SMALL_BLOCK_SIZE {
        return finish_block(data.to_vec(), block_len, true, 0, checksum);
    }

    // Stage 1: LZP (real implementation, see lzp.rs)
    let mut lzp0_dst = vec![0u8; LzpCodec::max_encoded_len(block_len)];
    let (stage1_out, skip_lzp0): (Vec<u8>, u8) = match lzp0.forward(data, &mut lzp0_dst) {
        Ok((_, n)) => {
            lzp0_dst.truncate(n);
            (lzp0_dst, 0)
        }
        Err(_) => (data.to_vec(), 1),
    };

    // Stage 2: TEXT (codec 1 for CM/FPAQ/TPAQ; see Factory.go newToken).
    let mut text_dst = vec![0u8; text_codec::max_encoded_len(stage1_out.len())];
    let (stage2_out, skip_text, dt_text): (Vec<u8>, u8, DataType) =
        match text_codec1::forward(&stage1_out, &mut text_dst, block_size, false) {
            Ok((_, n, dt)) => {
                text_dst.truncate(n);
                (text_dst, 0, dt)
            }
            Err((_, dt)) => (stage1_out.clone(), 1, dt),
        };

    // Stage 3: UTF (real implementation, see utf.rs)
    let mut utf_dst = vec![0u8; utf::max_encoded_len(stage2_out.len())];
    let (stage3_out, skip_utf): (Vec<u8>, u8) = match utf::forward(&stage2_out, &mut utf_dst, dt_text) {
        Ok((_, n, _)) => {
            utf_dst.truncate(n);
            (utf_dst, 0)
        }
        Err(_) => (stage2_out.clone(), 1),
    };

    // Stage 4: BWT (real implementation, see bwt.rs)
    let mut bwt_dst = vec![0u8; crate::bwt::max_encoded_len(stage3_out.len())];
    let (stage4_out, skip_bwt): (Vec<u8>, u8) = match bwt.forward(&stage3_out, &mut bwt_dst) {
        Ok((_, n)) => {
            bwt_dst.truncate(n);
            (bwt_dst, 0)
        }
        Err(_) => (stage3_out.clone(), 1),
    };

    // Stage 5: LZP again (fresh state -- second slot in the sequence)
    let mut lzp1_dst = vec![0u8; LzpCodec::max_encoded_len(stage4_out.len())];
    let (stage5_out, skip_lzp1): (Vec<u8>, u8) = match lzp1.forward(&stage4_out, &mut lzp1_dst) {
        Ok((_, n)) => {
            lzp1_dst.truncate(n);
            (lzp1_dst, 0)
        }
        Err(_) => (stage4_out.clone(), 1),
    };

    // 5-transform sequence: top 5 skip bits used, low 3 stay set.
    let skip_flags: u8 =
        (skip_lzp0 << 7) | (skip_text << 6) | (skip_utf << 5) | (skip_bwt << 4) | (skip_lzp1 << 3) | 0x07;

    // CM entropy (fresh predictor per block, disposed at block end).
    let mut cenc = BinaryEntropyEncoder::new(CmPredictor::new());
    let mut ebw = BitWriter::new();
    cenc.write(&stage5_out, &mut ebw).expect("cm encode");
    cenc.dispose(&mut ebw);
    let entropy_payload = ebw.finish_with_len();

    let normal = finish_block_multi(entropy_payload, stage5_out.len(), false, skip_flags, 5, checksum);
    maybe_transformed_copy(normal, &stage5_out, skip_flags, 5, checksum)
}

/// Encodes `data` as a complete level-8 (EXE+RLT+TEXT+UTF+DNA & TPAQ)
/// kanzi bitstream. All stages are fully ported.
pub fn encode_level8(data: &[u8], block_size: u32, ck_size: u64) -> Vec<u8> {
    encode_level89(data, block_size, false, ck_size)
}

/// Encodes `data` as a complete level-9 (EXE+RLT+TEXT+UTF+DNA & TPAQX)
/// kanzi bitstream. Identical transform chain to level 8 -- only the
/// entropy stage differs (TPAQX: a second SSE stage plus a 7th mixer
/// input from an extra hashed context; see tpaq.rs).
pub fn encode_level9(data: &[u8], block_size: u32, ck_size: u64) -> Vec<u8> {
    encode_level89(data, block_size, true, ck_size)
}

fn encode_level89(data: &[u8], block_size: u32, extra: bool, ck_size: u64) -> Vec<u8> {
    let hasher = Hasher::new(ck_size).expect("invalid ck_size");
    let mut bw = BitWriter::new();
    let transform_type: u64 = (EXE_TYPE << BFF_MAX_SHIFT)
        | (RLT_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (TEXT_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (UTF_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (DNA_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    let entropy_type = if extra { TPAQX_ENTROPY } else { TPAQ_ENTROPY };
    write_stream_header(&mut bw, entropy_type, transform_type, block_size, &hasher);

    let blocks = encode_blocks_parallel(data, block_size, || (), |_state, block| {
        encode_block89(block, block_size, extra, hasher.checksum(block))
    });

    for (encoded_block, written) in &blocks {
        write_framed_block(&mut bw, encoded_block, *written);
    }

    bw.write_bits(0, 5);
    bw.write_bits(0, 3);

    bw.finish()
}

fn encode_block89(data: &[u8], block_size: u32, extra: bool, checksum: Option<(u64, u8)>) -> (Vec<u8>, u64) {
    let block_len = data.len();

    if block_len <= SMALL_BLOCK_SIZE {
        return finish_block(data.to_vec(), block_len, true, 0, checksum);
    }

    // Stage 1: EXE (real implementation, see exe.rs). First in the chain,
    // so it gets no dt hint from a prior stage (Undefined -> self-detects).
    let mut exe_dst = vec![0u8; exe::max_encoded_len(block_len)];
    let (stage1_out, skip_exe, dt_exe): (Vec<u8>, u8, DataType) = match exe::forward(data, &mut exe_dst, DataType::Undefined) {
        Ok((_, n, dt)) => {
            exe_dst.truncate(n);
            (exe_dst, 0, dt)
        }
        Err((_, dt)) => (data.to_vec(), 1, dt),
    };

    // Stage 2: RLT (real implementation, see rlt.rs)
    let mut rlt_dst = vec![0u8; rlt::max_encoded_len(stage1_out.len())];
    let (stage2_out, skip_rlt, _dt_rlt): (Vec<u8>, u8, DataType) = match rlt::forward(&stage1_out, &mut rlt_dst, dt_exe) {
        Ok((_, n, dt)) => {
            rlt_dst.truncate(n);
            (rlt_dst, 0, dt)
        }
        Err((_, dt)) => (stage1_out.clone(), 1, dt),
    };

    // Stage 3: TEXT (codec 1 for CM/FPAQ/TPAQ/TPAQX; see Factory.go newToken).
    // `extra` (entropy_tpaqx) affects the dictionary hash table sizing
    // (text_codec1.rs's log_hash_size), so it must match the real
    // entropy stage or the encoder/decoder will disagree on word indices.
    let mut text_dst = vec![0u8; text_codec::max_encoded_len(stage2_out.len())];
    let (stage3_out, skip_text, dt_text): (Vec<u8>, u8, DataType) =
        match text_codec1::forward(&stage2_out, &mut text_dst, block_size, extra) {
            Ok((_, n, dt)) => {
                text_dst.truncate(n);
                (text_dst, 0, dt)
            }
            Err((_, dt)) => (stage2_out.clone(), 1, dt),
        };

    // Stage 4: UTF (real implementation, see utf.rs). The data type threads
    // through to DNA below (Go's ctx["dataType"]).
    let mut utf_dst = vec![0u8; utf::max_encoded_len(stage3_out.len())];
    let (stage4_out, skip_utf, dt_utf): (Vec<u8>, u8, DataType) = match utf::forward(&stage3_out, &mut utf_dst, dt_text) {
        Ok((_, n, dt)) => {
            utf_dst.truncate(n);
            (utf_dst, 0, dt)
        }
        Err((_, dt)) => (stage3_out.clone(), 1, dt),
    };

    // Stage 5: DNA/Alias (real implementation, see alias.rs; onlyDNA=true)
    let mut dna_dst = vec![0u8; alias::max_encoded_len(stage4_out.len())];
    let (stage5_out, skip_dna): (Vec<u8>, u8) = match alias::forward(&stage4_out, &mut dna_dst, dt_utf, true) {
        Ok((_, n, _)) => {
            dna_dst.truncate(n);
            (dna_dst, 0)
        }
        Err(_) => (stage4_out.clone(), 1),
    };

    // 5-transform sequence: top 5 skip bits used, low 3 stay set.
    let skip_flags: u8 =
        (skip_exe << 7) | (skip_rlt << 6) | (skip_text << 5) | (skip_utf << 4) | (skip_dna << 3) | 0x07;

    // TPAQ/TPAQX entropy (fresh predictor per block, disposed at block end).
    let mut tenc = BinaryEntropyEncoder::new(TpaqPredictor::new(block_size, stage5_out.len() as u32, extra));
    let mut ebw = BitWriter::new();
    tenc.write(&stage5_out, &mut ebw).expect("tpaq encode");
    tenc.dispose(&mut ebw);
    let entropy_payload = ebw.finish_with_len();

    let normal = finish_block_multi(entropy_payload, stage5_out.len(), false, skip_flags, 5, checksum);
    maybe_transformed_copy(normal, &stage5_out, skip_flags, 5, checksum)
}

fn log2_no_check(x: u32) -> u32 {
    31 - x.leading_zeros()
}

// ---------------------------------------------------------------------
// Decoder: reads a level-1/2/3/4/5/6 .knz stream produced either by this
// project's own encoder or by the real kanzi Go CLI/library, as long as
// the transform sequence matches one of the five combos this project
// knows how to encode (LZX-only; DNA+LZ; TEXT+UTF+PACK+MM+LZX;
// TEXT+UTF+EXE+PACK+MM+ROLZ; TEXT+UTF+BWT+RANK+ZRLT). Any other
// transform_type, or a checksummed stream (ckSize != 0 -- would need an
// XXHash32/64 port that isn't done here), is reported as an explicit
// error rather than silently mis-decoded.
// ---------------------------------------------------------------------

const MIN_BITSTREAM_BLOCK_SIZE: u32 = 1024;
const MAX_BITSTREAM_BLOCK_SIZE: u32 = 1024 * 1024 * 1024;
const TRANSFORMED_COPY_MASK: u8 = 0x10; // _TRANSFORMS_MASK, reused meaning when copyBlock is set
const COPY_BLOCK_MASK: u8 = 0x80;

struct StreamHeader {
    entropy_type: u64,
    transform_type: u64,
    block_size: u32,
    hasher: Hasher,
}

fn read_stream_header(br: &mut BitReader) -> Result<StreamHeader, String> {
    let magic = br.read_bits(32);

    if magic != BITSTREAM_TYPE {
        return Err(format!("Invalid stream type: {:#x}", magic));
    }

    let bs_version = br.read_bits(4);

    if bs_version != BITSTREAM_FORMAT_VERSION {
        return Err(format!(
            "Unsupported bitstream version: {} (only version 7 is supported)",
            bs_version
        ));
    }

    let ck_size = br.read_bits(2);
    let hasher = Hasher::new(ck_size).map_err(|_| {
        format!(
            "Invalid bitstream, incorrect checksum size: {} (ckSize must be 0, 1, or 2)",
            ck_size
        )
    })?;

    let entropy_type = br.read_bits(5);
    let transform_type = br.read_bits(48);
    let block_size = (br.read_bits(28) as u32) << 4;

    if block_size < MIN_BITSTREAM_BLOCK_SIZE || block_size > MAX_BITSTREAM_BLOCK_SIZE {
        return Err(format!(
            "Invalid bitstream, incorrect block size: {}",
            block_size
        ));
    }

    let sz_mask = br.read_bits(2);
    let output_size: u64 = if sz_mask != 0 {
        br.read_bits(16 * sz_mask as u32)
    } else {
        0
    };

    br.read_bits(15); // padding

    let cksum1 = br.read_bits(24) as u32;
    let seed = 0x01030507u32.wrapping_mul(bs_version as u32);
    let mut cksum2 = HASH.wrapping_mul(seed);
    cksum2 = mix32(cksum2, HASH, ck_size as u32);
    cksum2 = mix32(cksum2, HASH, entropy_type as u32);
    cksum2 = mix32(cksum2, HASH, (transform_type >> 32) as u32);
    cksum2 = mix32(cksum2, HASH, transform_type as u32);
    cksum2 = mix32(cksum2, HASH, block_size);

    if sz_mask > 0 {
        cksum2 = mix32(cksum2, HASH, (output_size >> 32) as u32);
        cksum2 = mix32(cksum2, HASH, output_size as u32);
    }

    cksum2 = (cksum2 >> 23) ^ (cksum2 >> 3);

    if cksum1 != (cksum2 & 0x00FF_FFFF) {
        return Err("Invalid bitstream: header checksum mismatch".to_string());
    }

    Ok(StreamHeader {
        entropy_type,
        transform_type,
        block_size,
        hasher,
    })
}

struct BlockHeader {
    raw_copy: bool,         // copy block, no transform/entropy at all (tiny blocks)
    transformed_copy: bool, // copy of the post-transform bytes; entropy bypassed but the transform inverse still runs
    skip_flags: u8,
    pre_transform_length: u32,
    header_bits: u64,
}

/// Number of transform slots for the five transform_type combos this
/// project knows how to encode/decode -- needed to pick the short-header
/// form (skip nibble folded into `mode`) vs. long-header form (separate
/// skipFlags byte), exactly as Go's `tSeq.Len()` does, without needing a
/// generic Transform/Sequence abstraction.
fn num_transforms_for(transform_type: u64) -> Result<usize, String> {
    if transform_type == 0 {
        // Level 0 (store): Go's Factory keeps one forced identity slot
        // even when the whole transform_type is NONE (nbtr==0 -> nbtr=1).
        return Ok(1);
    }

    let l1_type: u64 = LZX_TYPE << BFF_MAX_SHIFT;
    let l2_type: u64 = (DNA_TYPE << BFF_MAX_SHIFT) | (LZ_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT));
    let l3_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (PACK_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (MM_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (LZX_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    let l4_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (EXE_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (PACK_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (MM_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT))
        | (ROLZ_TYPE << (BFF_MAX_SHIFT - 5 * BFF_ONE_SHIFT));
    let l5_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (BWT_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (RANK_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (ZRLT_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    let l6_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (BWT_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (SRT_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (ZRLT_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    let l7_type: u64 = (LZP_TYPE << BFF_MAX_SHIFT)
        | (TEXT_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (UTF_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (BWT_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (LZP_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    let l89_type: u64 = (EXE_TYPE << BFF_MAX_SHIFT)
        | (RLT_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (TEXT_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (UTF_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (DNA_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));

    if transform_type == l1_type {
        Ok(1)
    } else if transform_type == l2_type {
        Ok(2)
    } else if transform_type == l3_type {
        Ok(5)
    } else if transform_type == l4_type {
        Ok(6)
    } else if transform_type == l5_type {
        Ok(5)
    } else if transform_type == l6_type {
        Ok(5)
    } else if transform_type == l7_type {
        Ok(5)
    } else if transform_type == l89_type {
        Ok(5)
    } else {
        Err(format!(
            "Unsupported transform_type {:#014x}: this decoder only supports the LZX-only (level 1), \
             DNA+LZ (level 2), TEXT+UTF+PACK+MM+LZX (level 3), TEXT+UTF+EXE+PACK+MM+ROLZ (level 4), \
             TEXT+UTF+BWT+RANK+ZRLT (level 5), TEXT+UTF+BWT+SRT+ZRLT (level 6), \
             LZP+TEXT+UTF+BWT+LZP (level 7), and EXE+RLT+TEXT+UTF+DNA (levels 8/9) sequences",
            transform_type
        ))
    }
}

fn read_block_header(
    br: &mut BitReader,
    encoded_block_length: u64,
    transform_type: u64,
) -> Result<BlockHeader, String> {
    if encoded_block_length < 8 {
        return Err("Invalid block size".to_string());
    }

    let mode = br.read_bits(8) as u8;
    let copy_block = mode & COPY_BLOCK_MASK != 0;
    let transformed_copy = copy_block && (mode & TRANSFORMED_COPY_MASK != 0);
    let mut has_skip_flags = false;
    let mut skip_flags: u8 = 0;

    if transformed_copy {
        // The block was re-emitted as a raw copy of the post-transform
        // bytes (entropy coding would have expanded it), but the
        // transform sequence itself still ran -- its skip flags are
        // still meaningful and encoded exactly like a normal block's.
        if num_transforms_for(transform_type)? > 4 {
            has_skip_flags = true;
        } else {
            skip_flags = (mode << 4) | 0x0F;
        }
    } else if copy_block {
        // rawCopy: no transform, no entropy; skip_flags is unused downstream.
    } else if mode & TRANSFORMED_COPY_MASK != 0 {
        has_skip_flags = true;
    } else {
        skip_flags = (mode << 4) | 0x0F;
    }

    let data_size: u32 = 1 + (((mode >> 5) & 0x03) as u32);
    let mut header_size: u32 = 1 + data_size;

    if has_skip_flags {
        header_size += 1;
    }

    header_size += 1; // bsVersion >= 7: header checksum byte

    if encoded_block_length < (header_size as u64) << 3 {
        return Err("Invalid block size".to_string());
    }

    if has_skip_flags {
        skip_flags = br.read_bits(8) as u8;
    }

    let mut pre_transform_length: u32 = 0;

    for _ in 0..data_size {
        let b = br.read_bits(8) as u32;
        pre_transform_length = (pre_transform_length << 8) | b;
    }

    let header_checksum = br.read_bits(8) as u32;
    let seed = 0x01030507u32;
    let mut cksum = HASH.wrapping_mul(seed);
    cksum = mix32(cksum, HASH, mode as u32);
    cksum = mix32(cksum, HASH, skip_flags as u32);
    cksum = mix32(cksum, HASH, pre_transform_length);
    cksum = mix32(cksum, HASH, (encoded_block_length >> 32) as u32);
    cksum = mix32(cksum, HASH, encoded_block_length as u32);
    cksum = (cksum >> 23) ^ (cksum >> 3);

    if header_checksum != (cksum & 0xFF) {
        return Err("Invalid bitstream, block header checksum mismatch".to_string());
    }

    Ok(BlockHeader {
        raw_copy: copy_block && !transformed_copy,
        transformed_copy,
        skip_flags,
        pre_transform_length,
        header_bits: (header_size as u64) * 8,
    })
}

/// Applies the known inverse-transform chain for one of the five
/// transform_type combos this project's encoder produces. `buffer` is the
/// entropy-decoded bytes (the output of the LAST forward transform); it is
/// consumed, and freed as soon as the first inverse stage has read it, so a
/// block never holds it alongside the later stages' buffers. Returns the
/// recovered original block bytes.
fn apply_inverse_transforms(
    buffer: Vec<u8>,
    transform_type: u64,
    skip_flags: u8,
    block_size: u32,
    entropy_tpaqx: bool,
    pool: &BufPool,
) -> Result<Vec<u8>, String> {
    if transform_type == 0 {
        // Level 0 (store): the one forced slot is NONE_TYPE, a pure
        // identity -- always a no-op regardless of skip_flags.
        return Ok(buffer);
    }

    let l1_type: u64 = LZX_TYPE << BFF_MAX_SHIFT;
    let l2_type: u64 = (DNA_TYPE << BFF_MAX_SHIFT) | (LZ_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT));
    let l3_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (PACK_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (MM_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (LZX_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    let l4_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (EXE_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (PACK_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (MM_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT))
        | (ROLZ_TYPE << (BFF_MAX_SHIFT - 5 * BFF_ONE_SHIFT));
    let l5_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (BWT_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (RANK_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (ZRLT_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    let l6_type: u64 = (TEXT_TYPE << BFF_MAX_SHIFT)
        | (UTF_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (BWT_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (SRT_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (ZRLT_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    let l7_type: u64 = (LZP_TYPE << BFF_MAX_SHIFT)
        | (TEXT_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (UTF_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (BWT_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (LZP_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));
    let l89_type: u64 = (EXE_TYPE << BFF_MAX_SHIFT)
        | (RLT_TYPE << (BFF_MAX_SHIFT - BFF_ONE_SHIFT))
        | (TEXT_TYPE << (BFF_MAX_SHIFT - 2 * BFF_ONE_SHIFT))
        | (UTF_TYPE << (BFF_MAX_SHIFT - 3 * BFF_ONE_SHIFT))
        | (DNA_TYPE << (BFF_MAX_SHIFT - 4 * BFF_ONE_SHIFT));

    // Intermediate buffers must absorb stage expansion, like Go's
    // Sequence.MaxEncodedLen chaining: EXE may add count/50 and FSD up to
    // count/16 on top of the block size (the final original block itself
    // never exceeds block_size). Without this headroom a stage whose output
    // legitimately exceeds block_size (e.g. ROLZ fed by EXE output) would
    // either overflow or, worse, silently switch ROLZ to multi-chunk
    // decoding of a single-chunk stream.
    let dst_cap = block_size as usize + block_size as usize / 16 + 1024;

    if transform_type == l1_type {
        // slot0 = LZX. skip bit is the top bit of the (8-bit-wide, 1
        // transform) skip_flags byte.
        if skip_flags & 0x80 != 0 {
            return Ok(buffer);
        }

        let mut dst = pool.take(dst_cap);
        let (_, n) = LzxCodec::inverse(&buffer, &mut dst).map_err(|e| e.to_string())?;
        pool.give(buffer);
        dst.truncate(n);
        Ok(dst)
    } else if transform_type == l2_type {
        // slot0 = DNA/Alias, slot1 = LZ. Inverse order is the reverse of
        // forward order: LZ first, then DNA/Alias (see alias.rs).
        let stage = if skip_flags & 0x40 != 0 {
            buffer
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = LzxCodec::inverse(&buffer, &mut dst).map_err(|e| e.to_string())?;
            pool.give(buffer);
            dst.truncate(n);
            dst
        };

        if skip_flags & 0x80 != 0 {
            return Ok(stage);
        }

        let mut dst = pool.take(dst_cap);
        let (_, n) = alias::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
        pool.give(stage);
        dst.truncate(n);
        Ok(dst)
    } else if transform_type == l3_type {
        // slot0=TEXT, slot1=UTF, slot2=PACK, slot3=MM, slot4=LZX. All five
        // inverses are ported (see text_codec.rs, utf.rs, alias.rs, fsd.rs).
        // Inverse order is the reverse of forward order: LZX, MM, PACK, UTF,
        // TEXT.
        let skip_lzx = skip_flags & 0x08 != 0;
        let skip_mm = skip_flags & 0x10 != 0;
        let skip_pack = skip_flags & 0x20 != 0;
        let skip_utf = skip_flags & 0x40 != 0;
        let skip_text = skip_flags & 0x80 != 0;

        let trace = std::env::var("DECTRACE").is_ok();
        let t0 = std::time::Instant::now();

        let stage = if skip_lzx {
            buffer
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = LzxCodec::inverse(&buffer, &mut dst).map_err(|e| e.to_string())?;
            pool.give(buffer);
            dst.truncate(n);
            dst
        };
        let t_lzx = t0.elapsed();

        let t1 = std::time::Instant::now();
        let stage = if skip_mm {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = fsd::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };
        let t_mm = t1.elapsed();

        let t2 = std::time::Instant::now();
        let stage = if skip_pack {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = alias::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };
        let t_pack = t2.elapsed();

        let t3 = std::time::Instant::now();
        let stage = if skip_utf {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = utf::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };
        let t_utf = t3.elapsed();

        let t4 = std::time::Instant::now();
        let result = if skip_text {
            Ok(stage)
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, back_len) = text_codec::inverse(&stage, &mut dst, block_size, false)?;
            pool.give(stage);
            dst.truncate(back_len);
            Ok(dst)
        };
        let t_text = t4.elapsed();

        if trace {
            eprintln!(
                "DEC3 stages(us): lzx={} mm={} pack={} utf={} text={}",
                t_lzx.as_micros(),
                t_mm.as_micros(),
                t_pack.as_micros(),
                t_utf.as_micros(),
                t_text.as_micros()
            );
        }

        result
    } else if transform_type == l4_type {
        // slot0=TEXT, slot1=UTF, slot2=EXE, slot3=PACK, slot4=MM, slot5=ROLZ.
        // All six inverses are ported (see text_codec.rs, utf.rs, exe.rs,
        // alias.rs, fsd.rs, rolz.rs). Inverse order is the reverse of
        // forward order: ROLZ, MM, PACK, EXE, UTF, TEXT.
        let skip_rolz = skip_flags & 0x04 != 0;
        let skip_mm = skip_flags & 0x08 != 0;
        let skip_pack = skip_flags & 0x10 != 0;
        let skip_exe = skip_flags & 0x20 != 0;
        let skip_utf = skip_flags & 0x40 != 0;
        let skip_text = skip_flags & 0x80 != 0;

        let stage = if skip_rolz {
            buffer
        } else {
            let mut rolz = RolzCodec::new();
            let mut dst = pool.take(dst_cap);
            let (_, n) = rolz.inverse(&buffer, &mut dst).map_err(|e| e.to_string())?;
            pool.give(buffer);
            dst.truncate(n);
            dst
        };

        let stage = if skip_mm {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = fsd::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };

        let stage = if skip_pack {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = alias::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };

        let stage = if skip_exe {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = exe::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };

        let stage = if skip_utf {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = utf::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };

        if skip_text {
            Ok(stage)
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, back_len) = text_codec::inverse(&stage, &mut dst, block_size, false)?;
            pool.give(stage);
            dst.truncate(back_len);
            Ok(dst)
        }
    } else if transform_type == l5_type {
        // slot0=TEXT, slot1=UTF, slot2=BWT, slot3=RANK, slot4=ZRLT. All five
        // inverses are ported (see text_codec.rs, utf.rs, bwt.rs, sbrt.rs,
        // zrlt.rs). Inverse order is the reverse of forward order: ZRLT,
        // RANK, BWT, UTF, TEXT.
        let skip_zrlt = skip_flags & 0x08 != 0;
        let skip_rank = skip_flags & 0x10 != 0;
        let skip_bwt = skip_flags & 0x20 != 0;
        let skip_utf = skip_flags & 0x40 != 0;
        let skip_text = skip_flags & 0x80 != 0;

        let trace = std::env::var("DECTRACE").is_ok();
        let t0 = std::time::Instant::now();

        let stage = if skip_zrlt {
            buffer
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = zrlt::inverse(&buffer, &mut dst).map_err(|e| e.to_string())?;
            pool.give(buffer);
            dst.truncate(n);
            dst
        };
        let t_zrlt = t0.elapsed();

        let rank = Sbrt::new_rank();

        let t1 = std::time::Instant::now();
        let stage = if skip_rank {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = rank.inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };
        let t_rank = t1.elapsed();

        let t2 = std::time::Instant::now();
        let stage = if skip_bwt {
            stage
        } else {
            // In place: BWT's inverse needs no separate output buffer.
            let mut stage = stage;
            Bwt::new().inverse_in_place(&mut stage).map_err(|e| e.to_string())?;
            stage
        };
        let t_bwt = t2.elapsed();

        let t3 = std::time::Instant::now();
        let stage = if skip_utf {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = utf::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };
        let t_utf = t3.elapsed();

        let t4 = std::time::Instant::now();
        let result = if skip_text {
            Ok(stage)
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, back_len) = text_codec::inverse(&stage, &mut dst, block_size, false)?;
            pool.give(stage);
            dst.truncate(back_len);
            Ok(dst)
        };
        let t_text = t4.elapsed();

        if trace {
            eprintln!(
                "DEC5 stages(us): zrlt={} rank={} bwt={} utf={} text={}",
                t_zrlt.as_micros(),
                t_rank.as_micros(),
                t_bwt.as_micros(),
                t_utf.as_micros(),
                t_text.as_micros()
            );
        }

        result
    } else if transform_type == l6_type {
        // slot0=TEXT, slot1=UTF, slot2=BWT, slot3=SRT, slot4=ZRLT. All five
        // inverses are ported (see text_codec.rs, utf.rs, bwt.rs, srt.rs,
        // zrlt.rs). Inverse order is the reverse of forward order: ZRLT,
        // SRT, BWT, UTF, TEXT.
        let skip_zrlt = skip_flags & 0x08 != 0;
        let skip_srt = skip_flags & 0x10 != 0;
        let skip_bwt = skip_flags & 0x20 != 0;
        let skip_utf = skip_flags & 0x40 != 0;
        let skip_text = skip_flags & 0x80 != 0;

        let stage = if skip_zrlt {
            buffer
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = zrlt::inverse(&buffer, &mut dst).map_err(|e| e.to_string())?;
            pool.give(buffer);
            dst.truncate(n);
            dst
        };

        let srt = Srt::new();

        let stage = if skip_srt {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = srt.inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };

        let stage = if skip_bwt {
            stage
        } else {
            // In place: BWT's inverse needs no separate output buffer.
            let mut stage = stage;
            Bwt::new().inverse_in_place(&mut stage).map_err(|e| e.to_string())?;
            stage
        };

        let stage = if skip_utf {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = utf::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };

        if skip_text {
            Ok(stage)
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, back_len) = text_codec1::inverse(&stage, &mut dst, block_size, false)?;
            pool.give(stage);
            dst.truncate(back_len);
            Ok(dst)
        }
    } else if transform_type == l7_type {
        // slot0=LZP, slot1=TEXT, slot2=UTF, slot3=BWT, slot4=LZP. All five
        // inverses are ported (see lzp.rs, text_codec1.rs, utf.rs, bwt.rs).
        // Inverse order is the reverse of forward order: LZP(slot4), BWT,
        // UTF, TEXT, LZP(slot0) -- two independent LzpCodec instances, one
        // per slot, matching the encoder.
        let skip_lzp1 = skip_flags & 0x08 != 0;
        let skip_bwt = skip_flags & 0x10 != 0;
        let skip_utf = skip_flags & 0x20 != 0;
        let skip_text = skip_flags & 0x40 != 0;
        let skip_lzp0 = skip_flags & 0x80 != 0;

        let trace = std::env::var("DECTRACE").is_ok();
        let t0 = std::time::Instant::now();

        let stage = if skip_lzp1 {
            buffer
        } else {
            let mut lzp1 = LzpCodec::new();
            let mut dst = pool.take(dst_cap);
            let (_, n) = lzp1.inverse(&buffer, &mut dst).map_err(|e| e.to_string())?;
            pool.give(buffer);
            dst.truncate(n);
            dst
        };
        let t_lzp1 = t0.elapsed();

        let t1 = std::time::Instant::now();
        let stage = if skip_bwt {
            stage
        } else {
            // In place: BWT's inverse needs no separate output buffer.
            let mut stage = stage;
            Bwt::new().inverse_in_place(&mut stage).map_err(|e| e.to_string())?;
            stage
        };
        let t_bwt = t1.elapsed();

        let t2 = std::time::Instant::now();
        let stage = if skip_utf {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = utf::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };
        let t_utf = t2.elapsed();

        let t3 = std::time::Instant::now();
        let stage = if skip_text {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, back_len) = text_codec1::inverse(&stage, &mut dst, block_size, false)?;
            pool.give(stage);
            dst.truncate(back_len);
            dst
        };
        let t_text = t3.elapsed();

        let t4 = std::time::Instant::now();
        let result = if skip_lzp0 {
            Ok(stage)
        } else {
            let mut lzp0 = LzpCodec::new();
            let mut dst = pool.take(dst_cap);
            let (_, n) = lzp0.inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            Ok(dst)
        };
        let t_lzp0 = t4.elapsed();

        if trace {
            eprintln!(
                "DEC7 stages(us): lzp1={} bwt={} utf={} text={} lzp0={}",
                t_lzp1.as_micros(),
                t_bwt.as_micros(),
                t_utf.as_micros(),
                t_text.as_micros(),
                t_lzp0.as_micros()
            );
        }

        result
    } else if transform_type == l89_type {
        // slot0=EXE, slot1=RLT, slot2=TEXT, slot3=UTF, slot4=DNA/Alias. All
        // five inverses are ported (see exe.rs, rlt.rs, text_codec1.rs,
        // utf.rs, alias.rs). Inverse order is the reverse of forward order:
        // DNA, UTF, TEXT, RLT, EXE. Shared by levels 8 (TPAQ) and 9
        // (TPAQX) -- the transform chain is identical; only the entropy
        // stage (already decoded by the time this runs) differs.
        let skip_dna = skip_flags & 0x08 != 0;
        let skip_utf = skip_flags & 0x10 != 0;
        let skip_text = skip_flags & 0x20 != 0;
        let skip_rlt = skip_flags & 0x40 != 0;
        let skip_exe = skip_flags & 0x80 != 0;

        let stage = if skip_dna {
            buffer
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = alias::inverse(&buffer, &mut dst).map_err(|e| e.to_string())?;
            pool.give(buffer);
            dst.truncate(n);
            dst
        };

        let stage = if skip_utf {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = utf::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };

        let stage = if skip_text {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, back_len) = text_codec1::inverse(&stage, &mut dst, block_size, entropy_tpaqx)?;
            pool.give(stage);
            dst.truncate(back_len);
            dst
        };

        let stage = if skip_rlt {
            stage
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = rlt::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            dst
        };

        if skip_exe {
            Ok(stage)
        } else {
            let mut dst = pool.take(dst_cap);
            let (_, n) = exe::inverse(&stage, &mut dst).map_err(|e| e.to_string())?;
            pool.give(stage);
            dst.truncate(n);
            Ok(dst)
        }
    } else {
        Err(format!(
            "Unsupported transform_type {:#014x}: this decoder only supports the LZX-only (level 1), \
             DNA+LZ (level 2), TEXT+UTF+PACK+MM+LZX (level 3), TEXT+UTF+EXE+PACK+MM+ROLZ (level 4), \
             TEXT+UTF+BWT+RANK+ZRLT (level 5), TEXT+UTF+BWT+SRT+ZRLT (level 6), \
             LZP+TEXT+UTF+BWT+LZP (level 7), and EXE+RLT+TEXT+UTF+DNA (levels 8/9) sequences",
            transform_type
        ))
    }
}

/// Decodes one framed block, consuming its compressed bytes: they are freed
/// as soon as the entropy stage has produced the transform input, so a block
/// in flight never holds its compressed bytes and its transform buffers at
/// the same time.
fn decode_block(block: FramedBlock, hdr: &StreamHeader, debug: bool, pool: &BufPool) -> Result<Vec<u8>, String> {
    let (block_header, expected_checksum, payload) = {
        let mut br = BitReader::at_bit_pos(&block.bytes, block.bit_off);
        let br = &mut br;
        let block_header = read_block_header(br, block.written, hdr.transform_type)?;
        let pre_len = block_header.pre_transform_length as usize;

        if debug {
            eprintln!(
                "DEBUG   raw_copy={} transformed_copy={} skip_flags={:#010b} pre_transform_length={} header_bits={}",
                block_header.raw_copy, block_header.transformed_copy, block_header.skip_flags, pre_len, block_header.header_bits
            );
        }

        if pre_len == 0 || pre_len as u32 > hdr.block_size + hdr.block_size / 2 + 2048 {
            return Err(format!("Invalid compressed block size: {}", pre_len));
        }

        // The payload checksum (if any) sits right after the block header and
        // before the payload, in every block shape (raw copy, transformed
        // copy, and normal entropy-coded) -- see Go's "Extract checksum from
        // bit stream" step, which runs unconditionally before dispatching on
        // rawCopy/transformedCopy/normal.
        let expected_checksum: Option<u64> = match &hdr.hasher {
            Hasher::None => None,
            Hasher::H32(_) => Some(br.read_bits(32)),
            Hasher::H64(_) => Some(br.read_bits(64)),
        };

        let payload = if block_header.raw_copy {
            // No transform, no entropy: the payload is the final bytes as-is.
            let mut payload = pool.take(pre_len);
            br.read_array(&mut payload, 8 * pre_len);
            payload
        } else {
            decode_block_entropy(br, &block_header, hdr, pre_len, pool)?
        };

        (block_header, expected_checksum, payload)
    };

    pool.give(block.bytes);

    let decoded = if block_header.raw_copy {
        payload
    } else {
        apply_inverse_transforms(
            payload,
            hdr.transform_type,
            block_header.skip_flags,
            hdr.block_size,
            hdr.entropy_type == TPAQX_ENTROPY,
            pool,
        )?
    };

    if let Some(expected) = expected_checksum {
        let actual = match &hdr.hasher {
            Hasher::None => unreachable!(),
            Hasher::H32(h) => h.hash(&decoded) as u64,
            Hasher::H64(h) => h.hash(&decoded),
        };

        if actual != expected {
            return Err(format!(
                "Corrupted bitstream: expected checksum {:#x}, found {:#x}",
                expected, actual
            ));
        }
    }

    Ok(decoded)
}

/// Entropy-decodes a block's payload into the transform input (or copies it
/// through for a transformed-copy block).
fn decode_block_entropy(
    br: &mut BitReader,
    block_header: &BlockHeader,
    hdr: &StreamHeader,
    pre_len: usize,
    pool: &BufPool,
) -> Result<Vec<u8>, String> {
    let mut buffer = pool.take(pre_len);

    let trace = std::env::var("DECTRACE").is_ok();
    let t_entropy0 = std::time::Instant::now();

    if block_header.transformed_copy {
        // Entropy coding was bypassed for this block (it would have
        // expanded the data) but the transform sequence still ran, so the
        // payload is the raw post-transform bytes.
        br.read_array(&mut buffer, 8 * pre_len);
    } else {
        match hdr.entropy_type {
            NONE_ENTROPY => br.read_array(&mut buffer, 8 * pre_len),
            HUFFMAN_ENTROPY => {
                let mut dec = HuffmanDecoderV6::new();
                dec.decode(br, &mut buffer)?;
            }
            ANS0_ENTROPY => {
                let mut dec = AnsDecoder::new(0, None).map_err(|e| e.to_string())?;
                dec.read(br, &mut buffer)?;
            }
            FPAQ_ENTROPY => {
                let mut dec = FpaqDecoder::new();
                dec.read_block(br, &mut buffer)?;
            }
            CM_ENTROPY => {
                let mut dec = BinaryEntropyDecoder::new(CmPredictor::new());
                dec.read_block(br, &mut buffer)?;
            }
            TPAQ_ENTROPY => {
                let mut dec = BinaryEntropyDecoder::new(TpaqPredictor::new(hdr.block_size, pre_len as u32, false));
                dec.read_block(br, &mut buffer)?;
            }
            TPAQX_ENTROPY => {
                let mut dec = BinaryEntropyDecoder::new(TpaqPredictor::new(hdr.block_size, pre_len as u32, true));
                dec.read_block(br, &mut buffer)?;
            }
            other => return Err(format!("Unsupported entropy type: {}", other)),
        }
    }

    if trace {
        eprintln!("DEC entropy(us): type={} t={}", hdr.entropy_type, t_entropy0.elapsed().as_micros());
    }

    Ok(buffer)
}

/// Output is written in pieces of at most this size. On Windows a single
/// `WriteFile` of a multi-hundred-MB buffer goes 4-5x slower through the
/// cache manager than the same bytes in block-sized writes (255-338 ms vs
/// 57-73 ms for 212 MB on an i3-12100 / NVMe), and a block can be up to
/// 1 GiB with an explicit block size.
const WRITE_CHUNK: usize = 4 << 20;

/// Recycles block-sized byte buffers for the duration of one decode call.
///
/// Every block used to get fresh buffers -- its compressed bytes, the entropy
/// output, one per inverse-transform stage, the decoded output -- and Windows
/// serves and releases allocations this large straight from the OS, so every
/// block page-faulted all of them in again: 90K faults decoding silesia.tar
/// at level 1 on one thread, against kanzi-cpp's 4K, which reuses its
/// buffers. Buffers go back here once superseded and are handed out again to
/// any request they fit, up to four times its size -- within one stream the
/// buffers are all the same order of magnitude, so a block-output buffer can
/// also serve the next block's compressed bytes or entropy output.
struct BufPool {
    free: std::sync::Mutex<Vec<Vec<u8>>>,
    /// Upper bound on the capacity parked at once, in bytes.
    budget: usize,
}

impl BufPool {
    /// Smaller buffers are served from the allocator's own free lists anyway.
    const MIN_POOLED: usize = 256 * 1024;

    /// Parked-capacity budget for a decode. Measured on silesia.tar: 16 MiB
    /// takes an L1 one-thread decode from 90K page faults to 6K and 296 to
    /// 258 ms (L3: 720 -> 686 ms) for +3..7 MB peak; 8 MiB loses half of
    /// that and 32 MiB gains nothing more. A budget rather than a buffer
    /// count keeps 16 MiB blocks (levels 7-8) from parking two 18 MB
    /// buffers for a <1% gain.
    const DECODE_BUDGET: usize = 16 << 20;

    fn new(budget: usize) -> Self {
        BufPool {
            free: std::sync::Mutex::new(Vec::new()),
            budget,
        }
    }

    /// An empty parked buffer with capacity in `cap..=4 * cap`, if any.
    fn reuse(&self, cap: usize) -> Option<Vec<u8>> {
        if cap < Self::MIN_POOLED {
            return None;
        }

        let mut free = self.free.lock().unwrap();
        let i = (0..free.len())
            .filter(|&i| free[i].capacity() >= cap && free[i].capacity() <= 4 * cap)
            .min_by_key(|&i| free[i].capacity())?;
        let mut v = free.swap_remove(i);
        v.clear();
        Some(v)
    }

    /// A buffer of `len` zero bytes.
    fn take(&self, len: usize) -> Vec<u8> {
        match self.reuse(len) {
            Some(mut v) => {
                v.resize(len, 0);
                v
            }
            None => vec![0u8; len],
        }
    }

    /// An empty buffer with at least `cap` capacity.
    fn take_empty(&self, cap: usize) -> Vec<u8> {
        self.reuse(cap).unwrap_or_else(|| Vec::with_capacity(cap))
    }

    /// Parks `v` for reuse, within the byte budget: smaller parked buffers
    /// are evicted to make room (larger ones serve every request a smaller
    /// one could), and a buffer that still doesn't fit is simply freed.
    fn give(&self, v: Vec<u8>) {
        if v.capacity() < Self::MIN_POOLED || v.capacity() > self.budget {
            return;
        }

        let mut evicted = Vec::new();
        let mut free = self.free.lock().unwrap();
        let mut parked: usize = free.iter().map(|b| b.capacity()).sum();

        while parked + v.capacity() > self.budget {
            let Some(i) = (0..free.len())
                .filter(|&i| free[i].capacity() < v.capacity())
                .min_by_key(|&i| free[i].capacity())
            else {
                return;
            };

            parked -= free[i].capacity();
            evicted.push(free.swap_remove(i));
        }

        free.push(v);
        drop(free);
        drop(evicted);
    }
}

/// Decodes a complete .knz bitstream held in memory. Prefer [`decode_to`]
/// when the input is a file or the output goes to one: it never holds more
/// than a few blocks at once.
pub fn decode(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut blocks: Vec<Vec<u8>> = Vec::new();

    decode_with(data, None, |block| {
        blocks.push(block);
        Ok(None)
    })?;

    if blocks.len() == 1 {
        return Ok(blocks.pop().unwrap());
    }

    let mut out = Vec::with_capacity(blocks.iter().map(|b| b.len()).sum());

    for block in blocks {
        out.extend_from_slice(&block);
    }

    Ok(out)
}

/// Decodes a .knz bitstream from `input`, writing the original bytes to
/// `out` in order as blocks finish, and returns the number of bytes written.
///
/// Both sides stream: `input` is read one framed block at a time and blocks
/// are decoded in parallel, but only a small window of blocks (about two per
/// worker thread) is buffered at any point, so memory is bounded by the
/// stream's block size, not by the input or output size. Neither side is
/// buffered further: input is pulled in pieces of 64 KiB or a whole block,
/// output is written in pieces of up to 4 MiB, and `out` is not flushed.
///
/// On error, the blocks before the failing one have already been written.
pub fn decode_to<R: std::io::Read, W: std::io::Write>(input: R, out: &mut W) -> Result<u64, String> {
    let mut total = 0u64;

    decode_with(input, None, |block| {
        for chunk in block.chunks(WRITE_CHUNK) {
            out.write_all(chunk).map_err(|e| format!("write error: {}", e))?;
        }

        total += block.len() as u64;
        Ok(Some(block))
    })?;

    Ok(total)
}

/// Shared by [`decode`] and [`decode_to`]: reads the stream header, then
/// hands each decoded block to `emit` in stream order. `emit` returns the
/// block's buffer when it does not keep it, for reuse by later blocks.
/// `jobs` pins the worker thread count (tests run the same stream
/// sequentially and in parallel); `None` means `worker_count`.
fn decode_with<R: std::io::Read>(
    input: R,
    jobs: Option<usize>,
    mut emit: impl FnMut(Vec<u8>) -> Result<Option<Vec<u8>>, String>,
) -> Result<(), String> {
    let trace = std::env::var("DECTRACE").is_ok();
    let t_fn = std::time::Instant::now();
    let mut reader = BlockReader::new(input, std::env::var("KDEBUG").is_ok());
    let hdr = reader.read_header()?;
    let workers = jobs.unwrap_or_else(|| worker_count(usize::MAX)).max(1);
    let pool = BufPool::new(BufPool::DECODE_BUDGET);
    let mut d_emit = std::time::Duration::ZERO;

    decode_blocks_ordered(&mut reader, &hdr, workers, &pool, |block| {
        let t = std::time::Instant::now();
        let spare = emit(block);
        d_emit += t.elapsed();

        if let Some(buf) = spare? {
            pool.give(buf);
        }

        Ok(())
    })?;

    if trace {
        eprintln!(
            "DEC total(us): read={} emit={} fn={}",
            reader.read_time.as_micros(),
            d_emit.as_micros(),
            t_fn.elapsed().as_micros()
        );
    }

    Ok(())
}

/// One framed block as read off the stream: its bytes, and where its bits
/// start and how many there are. The first byte may carry trailing bits of
/// the previous block's length prefix, hence `bit_off`.
struct FramedBlock {
    bytes: Vec<u8>,
    bit_off: usize,
    written: u64,
}

/// Pulls the stream header and then one framed block at a time out of a
/// `Read`. Between blocks it holds only read-ahead for the next length
/// prefix (at most 64 KiB); a block body is read straight into that block's
/// own buffer.
///
/// Every block is a fully self-contained framed unit -- its own header,
/// checksum and entropy state, freshly (re)initialized on decode (see
/// encode_blocks_parallel's doc comment for why) -- so its bytes can be
/// decoded independently of every other block, on any thread.
struct BlockReader<R> {
    input: R,
    /// Read-ahead not yet consumed; `pos` is a bit offset into it.
    buf: Vec<u8>,
    pos: usize,
    /// Stream bit offset of `buf[0]`, for `KDEBUG` output.
    base_bits: u64,
    eof: bool,
    /// Largest acceptable framed block, from the stream header's block size.
    max_block_bytes: u64,
    blocks_read: usize,
    debug: bool,
    read_time: std::time::Duration,
}

impl<R: std::io::Read> BlockReader<R> {
    const READ_AHEAD: usize = 64 * 1024;

    fn new(input: R, debug: bool) -> Self {
        BlockReader {
            input,
            buf: Vec::new(),
            pos: 0,
            base_bits: 0,
            eof: false,
            max_block_bytes: 0,
            blocks_read: 0,
            debug,
            read_time: std::time::Duration::ZERO,
        }
    }

    /// Reads until `buf` holds at least `bytes` bytes or the input ends.
    fn fill_to(&mut self, bytes: usize) -> Result<(), String> {
        while self.buf.len() < bytes && !self.eof {
            let old = self.buf.len();
            self.buf.resize(old + (bytes - old).max(Self::READ_AHEAD), 0);

            match self.input.read(&mut self.buf[old..]) {
                Ok(0) => {
                    self.buf.truncate(old);
                    self.eof = true;
                }
                Ok(n) => self.buf.truncate(old + n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => self.buf.truncate(old),
                Err(e) => {
                    self.buf.truncate(old);
                    return Err(format!("read error: {}", e));
                }
            }
        }

        Ok(())
    }

    fn read_header(&mut self) -> Result<StreamHeader, String> {
        // The header is at most 208 bits; a shorter input reads as zeros
        // and fails the magic or header checksum check.
        self.fill_to(64)?;
        let mut br = BitReader::new(&self.buf);
        let hdr = read_stream_header(&mut br)?;
        self.pos = br.bits_read();

        // Same bound as kanzi-cpp's CompressedInputStream: a block's
        // pre-transform length is at most blockSize + blockSize/2 + 2048
        // (checked again in decode_block), plus its header and checksum.
        // Rejecting larger length prefixes up front keeps a corrupt or
        // hostile prefix from forcing a huge allocation.
        let bs = hdr.block_size as u64;
        self.max_block_bytes = bs + bs / 2 + 2048 + 64;

        Ok(hdr)
    }

    /// Reads the next framed block, or `None` at the end-of-stream marker.
    fn next_block(&mut self, pool: &BufPool) -> Result<Option<FramedBlock>, String> {
        let t = std::time::Instant::now();
        let r = self.next_block_inner(pool);
        self.read_time += t.elapsed();
        r
    }

    fn next_block_inner(&mut self, pool: &BufPool) -> Result<Option<FramedBlock>, String> {
        use std::io::Read;

        // Length prefix: 5 bits of width, then 3..=34 bits of block length.
        let offset = self.base_bits + self.pos as u64;
        self.fill_to((self.pos + 39).div_ceil(8))?;
        let mut br = BitReader::at_bit_pos(&self.buf, self.pos);
        let lw = (br.read_bits(5) as u32) + 3;
        let written = br.read_bits(lw);
        let prefix_end = br.bits_read();

        if prefix_end > self.buf.len() * 8 {
            return Err("Truncated bitstream: missing block length or end-of-stream marker".to_string());
        }

        self.pos = prefix_end;

        if written == 0 {
            return Ok(None);
        }

        self.blocks_read += 1;

        if self.debug {
            eprintln!(
                "DEBUG block {} offset={} lw={} written={} written_bytes={}",
                self.blocks_read,
                offset,
                lw,
                written,
                written.div_ceil(8)
            );
        }

        if written.div_ceil(8) > self.max_block_bytes {
            return Err(format!("Invalid block size: {} bits", written));
        }

        let start = self.pos / 8;
        let bit_off = self.pos % 8;
        let end_bits = self.pos as u64 + written;
        let total = end_bits.div_ceil(8) as usize - start;
        let next_byte = (end_bits / 8) as usize;
        let buffered_end = self.buf.len().min(start + total);

        let mut bytes = pool.take_empty(total.min(64 << 20));
        bytes.extend_from_slice(&self.buf[start..buffered_end]);

        if bytes.len() < total {
            // The body runs past the read-ahead: read the rest straight into
            // this block's buffer. Everything buffered has now been consumed,
            // except the block's last byte if the next length prefix starts
            // inside it.
            let missing = (total - bytes.len()) as u64;
            (&mut self.input)
                .take(missing)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("read error: {}", e))?;

            if bytes.len() < total {
                return Err(format!("Truncated bitstream: block {} is incomplete", self.blocks_read));
            }

            self.buf.clear();
            self.buf.extend_from_slice(&bytes[next_byte - start..]);
        } else {
            self.buf.drain(..next_byte);
        }

        self.base_bits += next_byte as u64 * 8;
        self.pos = (end_bits % 8) as usize;

        Ok(Some(FramedBlock { bytes, bit_off, written }))
    }
}

/// Extracts a human-readable message from a caught panic payload (the
/// `Err` side of `std::thread::Result`), matching the two shapes
/// `panic!`/`.expect()`/indexing panics normally produce.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn decode_framed_block(block: FramedBlock, hdr: &StreamHeader, debug: bool, pool: &BufPool) -> Result<Vec<u8>, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || decode_block(block, hdr, debug, pool)))
    .unwrap_or_else(|payload| {
        Err(format!("decoder worker thread panicked: {}", panic_message(payload.as_ref())))
    })
}

/// Work queue shared between the reading/emitting thread and block workers.
struct BlockQueue {
    jobs: std::collections::VecDeque<(usize, FramedBlock)>,
    /// No more jobs will be pushed; workers exit once the queue is empty.
    closed: bool,
    /// Give up now: workers exit without taking further jobs.
    stop: bool,
}

/// Reads blocks from `reader`, decodes them on `workers` threads, and passes
/// the results to `emit` strictly in stream order. Reading and emitting
/// happen on the calling thread; workers only decode.
///
/// Workers take blocks from a shared queue rather than a fixed split: block
/// decode cost is content-dependent (BWT/entropy work scales with how
/// compressible the data is, and a trailing partial block is smaller), so a
/// static split leaves workers idle behind one expensive block. A block is
/// only read once it is within `2 * workers` of the next one to emit, which
/// bounds queued, in-flight and decoded-but-unemitted blocks together --
/// this is what keeps [`decode_to`]'s memory independent of stream size.
///
/// Errors come out in stream order: once block `i` fails (to read or to
/// decode), nothing past `i` is read or decoded, earlier blocks still finish
/// and are emitted, and the error of the lowest failing index is returned.
///
/// Every decoder here runs on untrusted bytes and is hardened against
/// panicking on corrupt input, but as a last line of defense a panic inside
/// a block decode is caught and turned into that block's `Err` -- which also
/// guarantees every queued block produces a result, so the emitting thread
/// never waits on one whose worker died.
fn decode_blocks_ordered<R: std::io::Read>(
    reader: &mut BlockReader<R>,
    hdr: &StreamHeader,
    workers: usize,
    pool: &BufPool,
    mut emit: impl FnMut(Vec<u8>) -> Result<(), String>,
) -> Result<(), String> {
    use std::collections::BTreeMap;
    use std::sync::{mpsc, Condvar, Mutex};

    let debug = reader.debug;

    if workers <= 1 {
        while let Some(block) = reader.next_block(pool)? {
            emit(decode_framed_block(block, hdr, debug, pool)?)?;
        }

        return Ok(());
    }

    // Don't start threads for a single-block stream (the common case for
    // small in-memory inputs).
    let Some(first) = reader.next_block(pool)? else {
        return Ok(());
    };

    let second = match reader.next_block(pool) {
        Ok(Some(block)) => block,
        Ok(None) => return emit(decode_framed_block(first, hdr, debug, pool)?),
        Err(e) => {
            emit(decode_framed_block(first, hdr, debug, pool)?)?;
            return Err(e);
        }
    };

    let window = 2 * workers;
    let queue = Mutex::new(BlockQueue {
        jobs: std::collections::VecDeque::from([(0, first), (1, second)]),
        closed: false,
        stop: false,
    });
    let ready = Condvar::new();
    let (tx, rx) = mpsc::channel::<(usize, Result<Vec<u8>, String>)>();

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let (queue, ready) = (&queue, &ready);

            scope.spawn(move || loop {
                let job = {
                    let mut q = queue.lock().unwrap();

                    loop {
                        if q.stop {
                            break None;
                        }

                        if let Some(job) = q.jobs.pop_front() {
                            break Some(job);
                        }

                        if q.closed {
                            break None;
                        }

                        q = ready.wait(q).unwrap();
                    }
                };

                let Some((i, block)) = job else { break };
                let res = decode_framed_block(block, hdr, debug, pool);

                if tx.send((i, res)).is_err() {
                    break;
                }
            });
        }

        drop(tx);
        ready.notify_all();

        let mut pending: BTreeMap<usize, Result<Vec<u8>, String>> = BTreeMap::new();
        let mut next_index = 2;
        let mut emitted = 0;
        let mut fail_at = usize::MAX;
        let mut end_reached = false;

        let result = loop {
            while !end_reached && next_index < fail_at && next_index < emitted + window {
                match reader.next_block(pool) {
                    Ok(Some(block)) => {
                        queue.lock().unwrap().jobs.push_back((next_index, block));
                        ready.notify_one();
                    }
                    Ok(None) => end_reached = true,
                    Err(e) => {
                        pending.insert(next_index, Err(e));
                        fail_at = next_index;
                        end_reached = true;
                    }
                }

                if !end_reached || fail_at == next_index {
                    next_index += 1;
                }
            }

            if end_reached {
                queue.lock().unwrap().closed = true;
                ready.notify_all();
            }

            if let Some(res) = pending.remove(&emitted) {
                match res.and_then(&mut emit) {
                    Ok(()) => {
                        emitted += 1;
                        continue;
                    }
                    Err(e) => break Err(e),
                }
            }

            if end_reached && emitted == next_index {
                break Ok(());
            }

            let Ok((i, res)) = rx.recv() else {
                break Err("decoder workers stopped before every block was decoded".to_string());
            };

            if res.is_err() && i < fail_at {
                fail_at = i;
                queue.lock().unwrap().jobs.retain(|(j, _)| *j < i);
            }

            pending.insert(i, res);
        };

        queue.lock().unwrap().stop = true;
        ready.notify_all();
        result
    })
}

/// Encodes one block's local (byte-aligned) buffer:
/// [mode:1][preTransformLength: dataSize bytes][headerChecksum:1][payload].
fn encode_block(data: &[u8], lzx: &mut LzxCodec, checksum: Option<(u64, u8)>) -> (Vec<u8>, u64) {
    let block_len = data.len();

    if block_len <= SMALL_BLOCK_SIZE {
        return finish_block(data.to_vec(), block_len, true, 0, checksum);
    }

    let mut dst = vec![0u8; LzxCodec::max_encoded_len(block_len)];
    let (payload, skip_bit): (Vec<u8>, u8) = match lzx.forward(data, &mut dst, lzx::MIN_MATCH4) {
        Ok((_, n)) => {
            dst.truncate(n);
            (dst, 0)
        }
        Err(_) => (data.to_vec(), 1),
    };

    let skip_flags = (skip_bit << 7) | 0x7F;
    let normal = finish_block_multi((payload.clone(), payload.len() as u64 * 8), payload.len(), false, skip_flags, 1, checksum);
    // With NONE entropy the entropy stage never shrinks the payload, so Go
    // always re-emits such blocks in transformed-copy form (strict < rule).
    maybe_transformed_copy(normal, &payload, skip_flags, 1, checksum)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEVEL_ENCODERS: [fn(&[u8], u32, u64) -> Vec<u8>; 10] = [
        encode_level0,
        encode_level1,
        encode_level2,
        encode_level3,
        encode_level4,
        encode_level5,
        encode_level6,
        encode_level7,
        encode_level8,
        encode_level9,
    ];

    /// Encodes+decodes `data` at every level (default 4 MiB block, no
    /// checksum) and asserts a byte-exact round trip, naming the failing
    /// level on mismatch.
    fn assert_roundtrips_all_levels(data: &[u8]) {
        for (level, encode) in LEVEL_ENCODERS.iter().enumerate() {
            let encoded = encode(data, 4 * 1024 * 1024, 0);
            let decoded = decode(&encoded)
                .unwrap_or_else(|e| panic!("level {level}: decode failed: {e}"));
            assert_eq!(decoded, data, "level {level}: round-trip mismatch");
        }
    }

    #[test]
    fn roundtrip_empty() {
        assert_roundtrips_all_levels(&[]);
    }

    #[test]
    fn roundtrip_tiny() {
        assert_roundtrips_all_levels(b"hi");
    }

    #[test]
    fn roundtrip_all_same_byte() {
        assert_roundtrips_all_levels(&vec![0x42u8; 20_000]);
    }

    #[test]
    fn roundtrip_pseudo_random() {
        // A small xorshift64 PRNG so this test needs no external crate.
        let mut state: u64 = 0x853c_49e6_748f_ea9b;
        let mut next_byte = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state & 0xFF) as u8
        };
        let data: Vec<u8> = (0..65_536).map(|_| next_byte()).collect();
        assert_roundtrips_all_levels(&data);
    }

    #[test]
    fn roundtrip_readme() {
        // This project's own README as a real, non-synthetic text sample.
        assert_roundtrips_all_levels(include_bytes!("../../../README.md"));
    }

    #[test]
    fn regression_lzx_tail_match_distance_read() {
        // lzx.rs's LzxCodec::inverse used to unconditionally read 4 bytes
        // for a match's distance value (src[m_idx..m_idx+4]) and panic
        // ("range end index N out of range for slice of length N-1") when
        // the last match in a block left fewer than 4 bytes after m_idx.
        // Found via examples/benchmark.py on this exact file at level 3.
        assert_roundtrips_all_levels(include_bytes!("../../../README.md"));
    }

    #[test]
    fn regression_rlt_long_run_crosses_medium_form_threshold() {
        // rlt.rs's emit_run_length() shadowed (instead of reassigning) its
        // `run` parameter inside the medium/long-form branches, so a run of
        // roughly 227+ identical bytes -- the point where the wire format
        // switches from a 1-byte to a 2-byte encoded length -- wrote the
        // wrong low byte and corrupted the rest of the block on decode.
        // This run length was chosen to land past that threshold.
        let mut data = vec![b'A'; 200];
        data.extend(std::iter::repeat_n(0u8, 300));
        data.extend(vec![b'B'; 200]);
        assert_roundtrips_all_levels(&data);
    }

    #[test]
    fn roundtrip_text_like_content() {
        // Rust source (this project's own) as a second, differently-shaped
        // real-content sample: lots of ASCII, braces, and identifiers.
        assert_roundtrips_all_levels(include_bytes!("rlt.rs"));
    }

    #[test]
    fn roundtrip_crlf_text() {
        // text_codec.rs's inverse() has a fast bulk-copy path for the
        // common (LF-only) case that is deliberately skipped whenever a
        // block is CRLF-flagged (`st.is_crlf`), since an LF byte there
        // expands to two output bytes (CR+LF) and the fast path assumes a
        // strict 1:1 src->dst mapping -- this pins that gate down: CRLF
        // content must keep decoding correctly via the untouched
        // byte-at-a-time path, not just LF-only content.
        let mut state: u64 = 0xC0FF_EE00_1234_5678;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let words = [
            "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "hello", "world", "test", "data",
            "compress", "decompress", "kanzi",
        ];
        let mut text = String::new();

        while text.len() < 200_000 {
            for _ in 0..10 {
                text.push_str(words[(next() as usize) % words.len()]);
                text.push(' ');
            }
            text.push_str("\r\n");
        }

        assert_roundtrips_all_levels(text.as_bytes());
    }

    #[test]
    fn buf_pool_reuses_within_budget() {
        const M: usize = 1 << 20;
        let pool = BufPool::new(16 * M);

        // Fresh buffers are zeroed; small requests bypass the pool entirely.
        assert!(pool.take(4 * M).iter().all(|&b| b == 0));
        pool.give(vec![7u8; 1024]);
        assert!(pool.free.lock().unwrap().is_empty());

        // A parked buffer comes back zeroed, to any request it fits up to 4x.
        let mut a = pool.take(4 * M);
        a.fill(9);
        let a_ptr = a.as_ptr();
        pool.give(a);
        let b = pool.take(2 * M);
        assert_eq!(b.as_ptr(), a_ptr, "buffer was not reused");
        assert!(b.len() == 2 * M && b.iter().all(|&x| x == 0));
        pool.give(b);
        assert!(pool.take_empty(M / 2).capacity() < 4 * M, "a 4 MiB buffer must not serve a 512 KiB request");

        // The budget holds; smaller parked buffers make way for larger ones.
        pool.give(vec![0u8; 6 * M]);
        pool.give(vec![0u8; 10 * M]);
        let parked = |p: &BufPool| p.free.lock().unwrap().iter().map(|v| v.capacity()).sum::<usize>();
        assert!(parked(&pool) <= 16 * M);
        pool.give(vec![0u8; 12 * M]);
        assert!(parked(&pool) <= 16 * M);
        assert!(pool.free.lock().unwrap().iter().any(|v| v.capacity() >= 12 * M), "larger buffer should evict smaller ones");
        pool.give(vec![0u8; 17 * M]);
        assert!(pool.free.lock().unwrap().iter().all(|v| v.capacity() < 17 * M), "over-budget buffer must not be parked");
    }

    /// Hands out at most `chunk` bytes per `read`, so block bodies and
    /// length prefixes straddle read boundaries at arbitrary bit offsets.
    struct ChunkedReader<'a> {
        data: &'a [u8],
        chunk: usize,
    }

    impl std::io::Read for ChunkedReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.chunk.min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    /// Text-like content that compresses, so blocks vary in encoded size.
    fn text_like(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        let words: [&[u8]; 8] = [b"alpha ", b"beta ", b"gamma ", b"delta\n", b"kanzi ", b"block ", b"stream ", b"42 "];
        let mut data = Vec::with_capacity(len + 8);

        while data.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.extend_from_slice(words[(state % 8) as usize]);
        }

        data.truncate(len);
        data
    }

    /// Runs the streaming decoder with a pinned thread count, returning
    /// everything emitted and the final result.
    fn decode_streaming(input: impl std::io::Read, jobs: usize) -> (Vec<u8>, Result<(), String>) {
        let mut out = Vec::new();
        let res = decode_with(input, Some(jobs), |block| {
            out.extend_from_slice(&block);
            Ok(Some(block))
        });
        (out, res)
    }

    #[test]
    fn streaming_decode_many_blocks_any_jobs_any_read_size() {
        // 1 KiB blocks: ~300 blocks, far more than the 2 * workers window,
        // so the read-ahead bound and out-of-order completion are exercised.
        let data = text_like(300_000, 0x5eed_0001);

        for (level, encode) in [(1, encode_level1 as fn(&[u8], u32, u64) -> Vec<u8>), (3, encode_level3), (6, encode_level6)] {
            let encoded = encode(&data, 1024, 1);

            for jobs in [1, 2, 8] {
                for chunk in [7, 4096, usize::MAX] {
                    let (out, res) = decode_streaming(ChunkedReader { data: &encoded, chunk }, jobs);
                    res.unwrap_or_else(|e| panic!("level {level} jobs {jobs} chunk {chunk}: {e}"));
                    assert!(out == data, "level {level} jobs {jobs} chunk {chunk}: output mismatch");
                }
            }

            let mut sink = Vec::new();
            assert_eq!(decode_to(encoded.as_slice(), &mut sink).unwrap(), data.len() as u64);
            assert!(sink == data, "level {level}: decode_to mismatch");
            assert!(decode(&encoded).unwrap() == data, "level {level}: decode mismatch");
        }
    }

    #[test]
    fn streaming_decode_errors_in_stream_order_regardless_of_jobs() {
        let data = text_like(200_000, 0x5eed_0002);
        let encoded = encode_level1(&data, 1024, 1);

        // Corrupt two spots; with per-block XXH32 checksums both are
        // detected, and every thread count must report the earlier one and
        // emit exactly the blocks before it.
        for spots in [[0.35, 0.70], [0.70, 0.35], [0.50, 0.51]] {
            let mut corrupt = encoded.clone();

            for f in spots {
                let at = (corrupt.len() as f64 * f) as usize;
                corrupt[at] ^= 0x5A;
            }

            let (out1, res1) = decode_streaming(corrupt.as_slice(), 1);
            let err1 = res1.expect_err("corruption must be detected");
            assert!(data.starts_with(&out1), "emitted output must be a prefix of the original");

            for jobs in [2, 8] {
                let (out, res) = decode_streaming(ChunkedReader { data: &corrupt, chunk: 777 }, jobs);
                assert_eq!(res, Err(err1.clone()), "jobs {jobs}: different error than sequential decode");
                assert!(out == out1, "jobs {jobs}: different emitted prefix than sequential decode");
            }
        }
    }

    #[test]
    fn streaming_decode_truncated_input_is_an_error() {
        let data = text_like(100_000, 0x5eed_0003);
        let encoded = encode_level1(&data, 1024, 0);

        for cut in [encoded.len() - 1, encoded.len() * 3 / 5, 40] {
            for jobs in [1, 8] {
                let (out, res) = decode_streaming(&encoded[..cut], jobs);
                assert!(res.is_err(), "cut at {cut}, jobs {jobs}: truncation not reported");
                assert!(data.starts_with(&out), "cut at {cut}, jobs {jobs}: emitted output not a prefix");
            }
        }
    }

    #[test]
    fn streaming_decode_stops_on_write_error() {
        struct FailingWriter {
            left: usize,
        }

        impl std::io::Write for FailingWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if self.left == 0 {
                    return Err(std::io::Error::other("disk full"));
                }

                let n = buf.len().min(self.left);
                self.left -= n;
                Ok(n)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let data = text_like(200_000, 0x5eed_0004);
        let encoded = encode_level3(&data, 1024, 0);
        let err = decode_to(encoded.as_slice(), &mut FailingWriter { left: 50_000 }).unwrap_err();
        assert!(err.contains("disk full"), "unexpected error: {err}");
    }

    #[test]
    fn streaming_decode_reads_ahead_only_a_bounded_window() {
        struct CountingReader<'a> {
            data: &'a [u8],
            read: std::rc::Rc<std::cell::Cell<usize>>,
        }

        impl std::io::Read for CountingReader<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = buf.len().min(self.data.len());
                buf[..n].copy_from_slice(&self.data[..n]);
                self.data = &self.data[n..];
                self.read.set(self.read.get() + n);
                Ok(n)
            }
        }

        // Incompressible 256 KiB blocks, larger than the reader's 64 KiB
        // read-ahead, so input consumed tracks blocks read.
        let block = 256 * 1024;
        let mut state = 0x5eed_0005u64;
        let data: Vec<u8> = (0..32 * block)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let encoded = encode_level1(&data, block as u32, 0);

        for jobs in [1, 2, 4] {
            let read = std::rc::Rc::new(std::cell::Cell::new(0));
            let window = if jobs == 1 { 1 } else { 2 * jobs };
            let mut emitted = 0;
            let mut out = Vec::new();

            decode_with(CountingReader { data: &encoded, read: read.clone() }, Some(jobs), |b| {
                emitted += 1;
                let bound = (emitted + window + 1) * (block + 4096) + BlockReader::<&[u8]>::READ_AHEAD;
                assert!(
                    read.get() <= bound,
                    "jobs {jobs}: {} bytes read by block {emitted}, bound {bound}",
                    read.get()
                );
                out.extend_from_slice(&b);
                Ok(Some(b))
            })
            .unwrap();

            assert!(out == data);
        }
    }

    #[test]
    fn streaming_decode_rejects_oversized_block_length_prefix() {
        // A valid header followed by a length prefix claiming a block far
        // larger than the header's block size allows must be rejected
        // before any attempt to allocate or read that much.
        let encoded = encode_level1(b"hello", 1024, 0);
        let mut reader = BlockReader::new(encoded.as_slice(), false);
        reader.read_header().unwrap();
        let header_bits = reader.pos;

        let mut bw = BitWriter::new();
        let mut br = BitReader::new(&encoded);
        let mut left = header_bits;

        while left > 0 {
            let n = left.min(32);
            bw.write_bits(br.read_bits(n as u32), n as u32);
            left -= n;
        }

        bw.write_bits(34 - 3, 5);
        bw.write_bits(1 << 33, 34);
        let forged = bw.finish();

        for jobs in [1, 8] {
            let (_, res) = decode_streaming(forged.as_slice(), jobs);
            let err = res.expect_err("oversized block must be rejected");
            assert!(err.contains("Invalid block size"), "unexpected error: {err}");
        }
    }
}
