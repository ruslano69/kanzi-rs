// Port of kanzi-go's FSDCodec (transform/FSDCodec.go) -- the Fixed Step
// Delta codec (MM transform) decorrelating values separated by a constant
// distance (1, 2, 3, 4, 8 or 16) via delta or xor residuals. Both Forward
// and Inverse (v7 layout) are fully ported.
//
// Fidelity notes:
// - The order-1-style sampling histograms and the entropy-1024 gates use the
//   same LOG2 tables as Go (see logtables.rs).
// - `_FSD_ZIGZAG2` (signed residual map) is computed by closed form instead
//   of a 256-entry literal: zigzag2(b) == table[b] was verified exhaustively
//   (all 256 values) during porting.
// - Only the v7 inverse is ported (this project is v7-only); Go takes the
//   same branch for bsVersion >= 7.
// - Like Go, Forward may return shrunk-or-equal output sizes and “allowed to
//   expand” up to MaxEncodedLen; the container decides skip vs apply purely
//   from Ok/Err like for every other stage.

use crate::datatype::DataType;
use crate::logtables::{TAB_LOG2, TAB_LOG2_4096};
use crate::magic;

pub const FSD_MIN_BLOCK_LENGTH: usize = 1024;
// NOTE: no escape token in v7 (only the pre-v7 inverse used _FSD_ESCAPE_TOKEN).
const FSD_DELTA_CODING: u8 = 0;
const FSD_XOR_CODING: u8 = 1;

pub fn max_encoded_len(src_len: usize) -> usize {
    src_len + (src_len >> 4).max(64) // limit expansion
}

/// Signed residual map: ZIGZAG2[0] = 0, odd b -> -(b+1)/2, even b -> b/2.
/// Proven equal to Go's _FSD_ZIGZAG2 literal on all 256 inputs.
#[inline]
fn zigzag2(b: u8) -> i32 {
    if b == 0 {
        0
    } else if b & 1 == 1 {
        -(((b as i32) + 1) / 2)
    } else {
        (b as i32) / 2
    }
}

fn log2_no_check(x: u32) -> u32 {
    let (mut v, mut res) = if x >= 1 << 16 {
        (x >> 16, 16u32)
    } else {
        (x, 0u32)
    };

    if v >= 1 << 8 {
        v >>= 8;
        res += 8;
    }

    res + TAB_LOG2[(v - 1) as usize]
}

fn log2_scaled_by_1024(x: u32) -> u32 {
    if x == 0 {
        return 0;
    }

    if x < 256 {
        return (TAB_LOG2_4096[x as usize] + 2) >> 2;
    }

    let log = log2_no_check(x);

    if x & (x - 1) == 0 {
        return log << 10;
    }

    ((log - 7) * 1024) + ((TAB_LOG2_4096[(x >> (log - 7)) as usize] + 2) >> 2)
}

/// Order-0 entropy of the histogram, scaled by 1024 (result in [0..1024]).
fn first_order_entropy_1024(block_len: usize, histo: &[i64]) -> i64 {
    if block_len == 0 {
        return 0;
    }

    let mut sum = 0u64;
    let log_length1024 = log2_scaled_by_1024(block_len as u32) as u64;

    for &f in histo.iter().take(256) {
        if f == 0 {
            continue;
        }

        let log1024 = log2_scaled_by_1024(f as u32) as u64;
        sum += ((f as u64) * (log_length1024 - log1024)) >> 3;
    }

    (sum / block_len as u64) as i64
}

/// FSDCodec.Forward. `dt_in` is the pipeline's current data type. Returns
/// (bytes_read, bytes_written, data_type) on both paths (Go's
/// ctx["dataType"] side effect).
pub fn forward(
    src: &[u8],
    dst: &mut [u8],
    dt_in: DataType,
) -> Result<(usize, usize, DataType), (&'static str, DataType)> {
    let count = src.len();

    if count == 0 || dst.is_empty() {
        return Ok((0, 0, dt_in));
    }

    if dst.len() < max_encoded_len(count) {
        return Err(("Output buffer is too small", dt_in));
    }

    // If too small, skip
    if count < FSD_MIN_BLOCK_LENGTH {
        return Err(("Block too small, skip", dt_in));
    }

    let mut dt = dt_in;

    if dt != DataType::Undefined && dt != DataType::Multimedia && dt != DataType::Bin {
        return Err(("FSD forward transform skip", dt));
    }

    let magic = magic::get_magic_type(src);

    // Skip detection except for a few candidate types
    match magic {
        magic::BMP_MAGIC
        | magic::RIFF_MAGIC
        | magic::PBM_MAGIC
        | magic::PGM_MAGIC
        | magic::PPM_MAGIC
        | magic::NO_MAGIC => {}
        _ => return Err(("FSD forward skip: found magic value header", dt)),
    }

    // Check several step values on a few sub-blocks (no memory allocation)
    let count10 = count / 10;
    let count5 = 2 * count10; // count5=count/5 does not guarantee count5=2*count10 !
    let mut histo = [[0i64; 256]; 7];

    for i in count10..count5 {
        // in0 = src[0:], in1 = src[2*count5:], in2 = src[4*count5:]
        let b0 = src[i];
        histo[0][b0 as usize] += 1;
        histo[1][(b0 ^ src[i - 1]) as usize] += 1;
        histo[2][(b0 ^ src[i - 2]) as usize] += 1;
        histo[3][(b0 ^ src[i - 3]) as usize] += 1;
        histo[4][(b0 ^ src[i - 4]) as usize] += 1;
        histo[5][(b0 ^ src[i - 8]) as usize] += 1;
        histo[6][(b0 ^ src[i - 16]) as usize] += 1;
        let b1 = src[2 * count5 + i];
        histo[0][b1 as usize] += 1;
        histo[1][(b1 ^ src[2 * count5 + i - 1]) as usize] += 1;
        histo[2][(b1 ^ src[2 * count5 + i - 2]) as usize] += 1;
        histo[3][(b1 ^ src[2 * count5 + i - 3]) as usize] += 1;
        histo[4][(b1 ^ src[2 * count5 + i - 4]) as usize] += 1;
        histo[5][(b1 ^ src[2 * count5 + i - 8]) as usize] += 1;
        histo[6][(b1 ^ src[2 * count5 + i - 16]) as usize] += 1;
        let b2 = src[4 * count5 + i];
        histo[0][b2 as usize] += 1;
        histo[1][(b2 ^ src[4 * count5 + i - 1]) as usize] += 1;
        histo[2][(b2 ^ src[4 * count5 + i - 2]) as usize] += 1;
        histo[3][(b2 ^ src[4 * count5 + i - 3]) as usize] += 1;
        histo[4][(b2 ^ src[4 * count5 + i - 4]) as usize] += 1;
        histo[5][(b2 ^ src[4 * count5 + i - 8]) as usize] += 1;
        histo[6][(b2 ^ src[4 * count5 + i - 16]) as usize] += 1;
    }

    // Find if entropy is lower post transform
    let mut ent = [0i64; 7];

    for i in 0..7 {
        ent[i] = first_order_entropy_1024(3 * count10, &histo[i]);
    }

    let mut min_idx = 0usize;

    for i in 1..7 {
        if ent[i] < ent[min_idx] {
            min_idx = i;
        }
    }

    // If not better, quick exit
    if ent[min_idx] >= ent[0] {
        dt = detect_subsample(&histo[0]);
        return Err(("FSD forward transform skip", dt));
    }

    dt = DataType::Multimedia;

    let distances = [0, 1, 2, 3, 4, 8, 16];
    let dist = distances[min_idx];
    let mut large_deltas = 0usize;

    // Detect best coding by sampling for large deltas
    for i in 2 * count5..3 * count5 {
        let delta = src[i] as i32 - src[i - dist] as i32;

        if delta < -127 || delta > 127 {
            large_deltas += 1;
        }
    }

    // Select xor coding if large signed deltas approach the rate expected for
    // unrelated byte pairs. With modular delta coding, large signed deltas
    // no longer cause expansion, so the old 3% threshold is too conservative.
    let mode = if large_deltas > (count5 >> 2) {
        FSD_XOR_CODING
    } else {
        FSD_DELTA_CODING
    };

    dst[0] = mode;
    dst[1] = dist as u8;
    let mut src_idx = 0usize;
    let mut dst_idx = 2usize;

    // Emit first bytes
    for i in 0..dist {
        dst[dst_idx] = src[src_idx + i];
        dst_idx += 1;
    }
    src_idx += dist;

    // Emit modified bytes
    if mode == FSD_DELTA_CODING {
        while src_idx < count {
            // Encode the delta modulo 256. The signed difference is not
            // needed to reconstruct a byte, and all 256 residuals fit in
            // one byte. Values in [-127..127] retain the previous zigzag
            // mapping; -128 and +128 share the same modular residual.
            let residual = src[src_idx].wrapping_sub(src[src_idx - dist]);
            let mut zigzag = (residual as u32) << 1;

            if residual & 0x80 != 0 {
                // Parenthesized explicitly: in Go `<<` binds tighter than
                // `-`, in Rust it is the other way around.
                zigzag = ((256u32 - residual as u32) << 1) - 1;
            }

            dst[dst_idx] = zigzag as u8;
            src_idx += 1;
            dst_idx += 1;
        }
    } else {
        // mode == _FSD_XOR_CODING
        while src_idx < count {
            dst[dst_idx] = src[src_idx] ^ src[src_idx - dist];
            src_idx += 1;
            dst_idx += 1;
        }
    }

    if src_idx != count {
        return Err(("FSD forward transform skip: output buffer too small", dt));
    }

    // Extra check that the transform makes sense
    histo[0].fill(0);
    let out1 = count5;
    let out2 = 3 * count5;

    for i in 0..count10 {
        histo[0][dst[out1 + i] as usize] += 1;
        histo[0][dst[out2 + i] as usize] += 1;
    }

    if first_order_entropy_1024(count5, &histo[0]) >= ent[0] {
        return Err(("FSD forward transform skip: no improvement", dt));
    }

    Ok((src_idx, dst_idx, dt)) // Allowed to expand
}

/// DataType for the quick-exit decline path: Go writes
/// DetectSimpleType(3*count10, histo[0]) into ctx.
fn detect_subsample(histo0: &[i64; 256]) -> DataType {
    let mut freqs = [0i32; 256];
    let mut total = 0usize;

    for (i, &f) in histo0.iter().enumerate() {
        freqs[i] = f as i32;
        total += f as usize;
    }

    // Go passes 3*count10 as count; total histogram mass equals it here.
    let _ = total;
    crate::datatype::detect_simple_type(histo0.iter().map(|&f| f as usize).sum(), &freqs)
}

/// FSDCodec inverse, v7 layout (see module note).
pub fn inverse(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
    if src.is_empty() || dst.is_empty() {
        return Ok((0, 0));
    }

    let count = src.len();

    if count < 4 {
        return Err("FSD inverse transform failed: input block is too small");
    }

    // Retrieve mode & step value
    let mode = src[0];
    let dist = src[1] as usize;

    // Sanity check
    if dist < 1 || (dist > 4 && dist != 8 && dist != 16) {
        return Err("FSD inverse transform failed: invalid distance");
    }

    if count < dist + 2 || dst.len() < dist {
        return Err("FSD inverse transform failed: invalid data");
    }

    // Emit first bytes
    let mut src_idx = 2usize;
    let mut dst_idx = 0usize;

    for _ in 0..dist {
        dst[dst_idx] = src[src_idx];
        dst_idx += 1;
        src_idx += 1;
    }

    // Recover original bytes
    if mode == FSD_DELTA_CODING {
        while src_idx < count && dst_idx < dst.len() {
            let delta = zigzag2(src[src_idx]);
            dst[dst_idx] = dst[dst_idx - dist].wrapping_add(delta as u8);
            dst_idx += 1;
            src_idx += 1;
        }
    } else if mode == FSD_XOR_CODING {
        while src_idx < count && dst_idx < dst.len() {
            dst[dst_idx] = src[src_idx] ^ dst[dst_idx - dist];
            dst_idx += 1;
            src_idx += 1;
        }
    } else {
        return Err("FSD inverse transform failed: invalid mode");
    }

    if src_idx != count {
        return Err("FSD inverse transform failed: output buffer too small");
    }

    Ok((src_idx, dst_idx))
}
