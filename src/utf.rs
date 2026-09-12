// Port of kanzi-go's UTFCodec (transform/UTFCodec.go) -- replaces UTF-8 code
// points with frequency-ranked short aliases. Both Forward and Inverse are
// fully ported.
//
// Fidelity notes:
// - `_UTF_SIZES` is computed by closed form (verified row-by-row against
//   Go's literal: <0x80 -> 1, <0xC2 -> 0, <0xE0 -> 2, <0xF0 -> 3, <0xF5 -> 4,
//   else 0).
// - The BOM probe is verbatim, including its off-by-three quirk: Go tests
//   bytes 1..4 (`BE32(src[0..4]) & 0x00FFFFFF`), not 0..3.
// - Inverse start/adjust are masked with &0x03 exactly like Go (so a stored
//   start of 4 would read back as 0, just like Go).
// - Only the v4+ (unpackUTF1) inverse layout is ported; this project is
//   v7-only and Go takes the same branch for versions 4 through 7.
// - Like Go, Forward allocates a 4M-entry alias map per call.

use crate::datatype::DataType;

pub const UTF_MIN_BLOCKSIZE: usize = 1024;

pub fn max_encoded_len(src_len: usize) -> usize {
    src_len + 8192
}

/// UTF-8 lead-byte sequence length (0 = invalid/continuation). Matches Go's
/// _UTF_SIZES table.
#[inline]
fn utf_size(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xC2 {
        0
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else if b < 0xF5 {
        4
    } else {
        0
    }
}

/// Packs the code point at `i` into Go's 22-bit (size + payload) layout.
/// Returns (packed, size); size 0 signals invalid. Caller guarantees the
/// 4-byte window is in bounds (loops run while i < count-4... i.e. i+3 <
// len, same as Go's packUTFAt contract).
fn pack_utf_at(src: &[u8], i: usize) -> (u32, usize) {
    match utf_size(src[i]) {
        1 => (src[i] as u32, 1),
        2 => ((1 << 19) | ((src[i] as u32) << 8) | src[i + 1] as u32, 2),
        3 => (
            (2 << 19)
                | (((src[i] as u32) & 0x0F) << 12)
                | (((src[i + 1] as u32) & 0x3F) << 6)
                | ((src[i + 2] as u32) & 0x3F),
            3,
        ),
        4 => (
            (4 << 19)
                | (((src[i] as u32) & 0x07) << 18)
                | (((src[i + 1] as u32) & 0x3F) << 12)
                | (((src[i + 2] as u32) & 0x3F) << 6)
                | ((src[i + 3] as u32) & 0x3F),
            4,
        ),
        _ => (0, 0),
    }
}

/// Unpacks a v4+ map entry into raw UTF-8 bytes. Returns the byte length,
/// 0 signals invalid (mirrors Go's unpackUTF1, sz==3 included).
fn unpack_utf1(packed: u32, out: &mut [u8; 4]) -> usize {
    match packed >> 19 {
        0 => {
            out[0] = packed as u8;
            1
        }
        1 => {
            out[0] = (packed >> 8) as u8;
            out[1] = packed as u8;
            2
        }
        2 => {
            out[0] = (((packed >> 12) & 0x0F) | 0xE0) as u8;
            out[1] = (((packed >> 6) & 0x3F) | 0x80) as u8;
            out[2] = ((packed & 0x3F) | 0x80) as u8;
            3
        }
        4..=7 => {
            out[0] = (((packed >> 18) & 0x07) | 0xF0) as u8;
            out[1] = (((packed >> 12) & 0x3F) | 0x80) as u8;
            out[2] = (((packed >> 6) & 0x3F) | 0x80) as u8;
            out[3] = ((packed & 0x3F) | 0x80) as u8;
            4
        }
        _ => 0,
    }
}

/// Quick partial validation (port of Go's validateUTF, same unrolled
/// counting, same early-exit rules, same sum2 >= count/8 threshold).
fn validate_utf(block: &[u8]) -> bool {
    let count = block.len();
    let mut freqs0 = [0i64; 256];
    let mut freqs1 = [[0i64; 256]; 256];
    let end4 = count & !3;
    let mut prv = 0usize;
    let mut i = 0usize;

    while i < end4 {
        let (c0, c1, c2, c3) = (block[i], block[i + 1], block[i + 2], block[i + 3]);
        freqs0[c0 as usize] += 1;
        freqs0[c1 as usize] += 1;
        freqs0[c2 as usize] += 1;
        freqs0[c3 as usize] += 1;
        freqs1[prv][c0 as usize] += 1;
        freqs1[c0 as usize][c1 as usize] += 1;
        freqs1[c1 as usize][c2 as usize] += 1;
        freqs1[c2 as usize][c3 as usize] += 1;
        prv = c3 as usize;
        i += 4;

        if (i - 4) & 0x0FFF == 0 {
            // Early check rules for 1 byte
            let mut sum = freqs0[0xC0] + freqs0[0xC1];

            for f in &freqs0[0xF5..] {
                sum += f;
            }

            if sum != 0 {
                return false;
            }
        }
    }

    if end4 != count {
        while i < count {
            let cur = block[i];
            freqs0[cur as usize] += 1;
            freqs1[prv][cur as usize] += 1;
            prv = cur as usize;
            i += 1;
        }

        // Check rules for 1 byte
        let mut sum = freqs0[0xC0] + freqs0[0xC1];

        for f in &freqs0[0xF5..] {
            sum += f;
        }

        if sum != 0 {
            return false;
        }
    }

    let mut sum = 0i64;
    let mut sum2 = 0i64;

    // Check rules for first 2 bytes
    for i in 0..256usize {
        // Exclude < 0xE0A0 || > 0xE0BF
        if i < 0xA0 || i > 0xBF {
            sum += freqs1[0xE0][i];
        }

        // Exclude < 0xED80 || > 0xED9F
        if i < 0x80 || i > 0x9F {
            sum += freqs1[0xED][i];
        }

        // Exclude < 0xF090 || > 0xF0BF
        if i < 0x90 || i > 0xBF {
            sum += freqs1[0xF0][i];
        }

        // Exclude < 0xF480 || > 0xF48F
        if i < 0x80 || i > 0x8F {
            sum += freqs1[0xF4][i];
        }

        if i < 0x80 || i > 0xBF {
            // Exclude < 0x??80 || > 0x??BF with ?? in [C2..DF]
            for j in 0xC2..=0xDF {
                sum += freqs1[j][i];
            }

            // Exclude < 0x??80 || > 0x??BF with ?? in [E1..EC]
            for j in 0xE1..=0xEC {
                sum += freqs1[j][i];
            }

            // Exclude < 0x??80 || > 0x??BF with ?? in [F1..F3]
            sum += freqs1[0xF1][i];
            sum += freqs1[0xF2][i];
            sum += freqs1[0xF3][i];

            // Exclude < 0xEE80 || > 0xEEBF
            sum += freqs1[0xEE][i];

            // Exclude < 0xEF80 || > 0xEFBF
            sum += freqs1[0xEF][i];
        } else {
            // Count non-primary bytes
            sum2 += freqs0[i];
        }

        if sum != 0 {
            return false;
        }
    }

    // Ad-hoc threshold
    sum2 >= (count as i64) / 8
}

/// UTFCodec.Forward. `dt_in` is the pipeline's current data type. Returns
/// (bytes_read, bytes_written, data_type) on both paths (Go's
/// ctx["dataType"] side effect -- note it becomes DT_UTF8 as soon as the
/// validation gate passes, even if a later gate declines).
pub fn forward(
    src: &[u8],
    dst: &mut [u8],
    dt_in: DataType,
) -> Result<(usize, usize, DataType), (&'static str, DataType)> {
    let count = src.len();

    if count == 0 || dst.is_empty() {
        return Ok((0, 0, dt_in));
    }

    if count < UTF_MIN_BLOCKSIZE {
        return Err(("Input block is too small", dt_in));
    }

    if dst.len() < max_encoded_len(count) {
        return Err(("Output buffer is too small", dt_in));
    }

    if dt_in != DataType::Undefined && dt_in != DataType::Utf8 {
        return Err(("UTF forward transform skip: not UTF", dt_in));
    }

    let must_validate = dt_in != DataType::Utf8;

    let mut start = 0usize;

    if u32::from_be_bytes(src[0..4].try_into().unwrap()) & 0x00FF_FFFF == 0x00EF_BBBF {
        // Byte Order Mark (BOM)
        start = 3;
    } else {
        // First (possibly) invalid symbols (due to block truncation).
        while start < 4 && utf_size(src[start]) == 0 {
            start += 1;
        }
    }

    if must_validate && !validate_utf(&src[start..count - 4]) {
        return Err(("UTF forward transform skip: not UTF", dt_in));
    }

    // From here on the data type is UTF8, even on later decline paths.
    let dt = DataType::Utf8;

    // 1-3 bit size + (7 or 11 or 16 or 21) bit payload
    // 3 MSBs indicate symbol size (limit map size to 22 bits)
    // 000 -> 7 bits, 001 -> 11 bits, 010 -> 16 bits, 1xx -> 21 bits
    let mut alias_map = vec![0i32; 1 << 22];
    let mut symb: Vec<(u32, i64)> = Vec::with_capacity(32768);
    let mut present = vec![false; 1 << 22];

    let mut i = start;

    while i < count - 4 {
        let (val, s) = pack_utf_at(src, i);
        let mut ok = s != 0;
        // Validation of longer sequences
        // Third byte in [0x80..0xBF]
        ok = ok && (s != 3 || (src[i + 2] & 0xC0) == 0x80);
        // Third and fourth bytes in [0x80..0xBF]
        ok =
            ok && (s != 4 || ((((src[i + 2] as u16) << 8) | src[i + 3] as u16) & 0xC0C0) == 0x8080);

        if !present[val as usize] {
            symb.push((val, 0));
            present[val as usize] = true;
            ok = ok && (symb.len() < 32768);
        }

        if !ok {
            return Err(("UTF forward transform skip: invalid or too complex", dt));
        }

        alias_map[val as usize] += 1;
        i += s;
    }

    let n = symb.len();

    if n == 0 {
        return Err(("UTF forward transform skip: not UTF", dt));
    }

    let max_target = count - (count / 10);

    if 3 * n + 6 >= max_target {
        return Err(("UTF forward transform skip: no improvement", dt));
    }

    for (val, freq) in symb.iter_mut() {
        *freq = alias_map[*val as usize] as i64;
    }

    // Sort ranks by increasing frequencies (total order via symbol tiebreak,
    // so stability is irrelevant -- matches SortStableFunc).
    symb.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    // Emit map length then map data (most frequent first)
    dst[2] = (n >> 8) as u8;
    dst[3] = (n & 0xFF) as u8;
    let mut dst_idx = 4usize;
    let mut estimate = dst_idx + 6;

    for (i, &(s, freq)) in symb.iter().rev().enumerate() {
        dst[dst_idx] = (s >> 16) as u8;
        dst[dst_idx + 1] = (s >> 8) as u8;
        dst[dst_idx + 2] = (s & 0xFF) as u8;
        dst_idx += 3;

        if i < 128 {
            estimate += freq as usize;
            alias_map[s as usize] = i as i32;
        } else {
            estimate += 2 * freq as usize;
            alias_map[s as usize] = 0x10080 | (((i << 1) & 0xFF00) as i32) | ((i & 0x7F) as i32);
        }
    }

    if estimate >= max_target {
        return Err(("UTF forward transform skip: no improvement", dt));
    }

    // Emit first (possibly) invalid symbols (due to block truncation)
    let mut raw = 0usize;

    while raw < start {
        dst[dst_idx] = src[raw];
        raw += 1;
        dst_idx += 1;
    }

    let mut src_idx = start;

    // Emit aliases
    while src_idx < count - 4 {
        let (val, s) = pack_utf_at(src, src_idx);
        src_idx += s;
        let alias = alias_map[val as usize];
        dst[dst_idx] = (alias & 0xFF) as u8;
        dst_idx += 1;
        dst[dst_idx] = ((alias >> 8) & 0xFF) as u8;
        dst_idx += (alias >> 16) as usize;
    }

    dst[0] = start as u8;
    dst[1] = (src_idx - (count - 4)) as u8;

    // Emit last (possibly) invalid symbols (due to block truncation)
    while src_idx < count {
        dst[dst_idx] = src[src_idx];
        src_idx += 1;
        dst_idx += 1;
    }

    if dst_idx >= max_target {
        return Err(("UTF forward transform skip: no improvement", dt));
    }

    Ok((src_idx, dst_idx, dt))
}

/// UTFCodec inverse, v7 (unpackUTF1) layout. `src` is the entropy-decoded
/// post-transform buffer.
pub fn inverse(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
    if src.is_empty() || dst.is_empty() {
        return Ok((0, 0));
    }

    let count = src.len();

    if count < 4 {
        return Err("Input block is too small");
    }

    let start = (src[0] as usize) & 0x03;
    let adjust = (src[1] as usize) & 0x03; // adjust end of regular processing
    let n = ((src[2] as usize) << 8) + src[3] as usize;

    // Protect against invalid map size value
    if n == 0 || n >= 32768 || 4 + 3 * n > count {
        return Err("UTF inverse transform: invalid map size");
    }

    // Build inverse mapping (v7 layout)
    let mut m_val = vec![[0u8; 4]; n];
    let mut m_len = vec![0u8; n];
    let mut src_idx = 4usize;

    for i in 0..n {
        let s = ((src[src_idx] as u32) << 16)
            | ((src[src_idx + 1] as u32) << 8)
            | src[src_idx + 2] as u32;
        let sl = unpack_utf1(s, &mut m_val[i]);

        if sl == 0 {
            return Err("UTF inverse transform failed: invalid UTF alias");
        }

        m_len[i] = sl as u8;
        src_idx += 3;
    }

    let src_end = count - 4 + adjust;

    if dst.len() < 4 {
        return Err("UTF inverse transform failed: invalid output block size");
    }

    if src_end < src_idx || src_end > count || src_idx + start > count {
        return Err("UTF inverse transform failed: invalid data");
    }

    let mut dst_idx = 0usize;

    for _ in 0..start {
        dst[dst_idx] = src[src_idx];
        src_idx += 1;
        dst_idx += 1;
    }

    // Emit data
    if n <= 128 {
        // All valid aliases fit in one byte.
        while src_idx < src_end {
            let alias = src[src_idx] as usize;
            src_idx += 1;

            if alias >= n {
                return Err("UTF inverse transform failed: invalid data");
            }

            // The symbol length controls the logical output advance, but the
            // decoder always copies four bytes from the packed symbol value.
            if dst_idx + 4 > dst.len() {
                return Err("UTF inverse transform failed: output buffer too small");
            }

            dst[dst_idx..dst_idx + 4].copy_from_slice(&m_val[alias]);
            dst_idx += m_len[alias] as usize;
        }
    } else {
        // Decode the next alias and load its value before storing the current
        // one. This allows independent dictionary lookups to overlap.
        while src_idx < src_end {
            let mut alias = src[src_idx] as usize;
            src_idx += 1;

            if alias >= 128 {
                if src_idx >= src_end {
                    return Err("UTF inverse transform failed: invalid data");
                }

                alias = ((src[src_idx] as usize) << 7) + (alias & 0x7F);
                src_idx += 1;
            }

            if alias >= n {
                return Err("UTF inverse transform failed: invalid data");
            }

            let (val0, len0) = (m_val[alias], m_len[alias] as usize);

            if src_idx >= src_end {
                if dst_idx + 4 > dst.len() {
                    return Err("UTF inverse transform failed: output buffer too small");
                }

                dst[dst_idx..dst_idx + 4].copy_from_slice(&val0);
                dst_idx += len0;
                break;
            }

            let mut alias2 = src[src_idx] as usize;
            src_idx += 1;

            if alias2 >= 128 {
                if src_idx >= src_end {
                    return Err("UTF inverse transform failed: invalid data");
                }

                alias2 = ((src[src_idx] as usize) << 7) + (alias2 & 0x7F);
                src_idx += 1;
            }

            if alias2 >= n {
                return Err("UTF inverse transform failed: invalid data");
            }

            let (val1, len1) = (m_val[alias2], m_len[alias2] as usize);
            let needed = len0 + len1 + 4;

            if dst.len() - dst_idx < needed {
                return Err("UTF inverse transform failed: output buffer too small");
            }

            dst[dst_idx..dst_idx + 4].copy_from_slice(&val0);
            dst_idx += len0;
            dst[dst_idx..dst_idx + 4].copy_from_slice(&val1);
            dst_idx += len1;
        }
    }

    // Signed comparison like Go (int arithmetic, no underflow panic).
    if src_idx < src_end || dst_idx as i64 > dst.len() as i64 - count as i64 + src_end as i64 {
        return Err("UTF inverse transform failed: invalid data");
    }

    for _ in src_end..count {
        dst[dst_idx] = src[src_idx];
        src_idx += 1;
        dst_idx += 1;
    }

    Ok((src_idx, dst_idx))
}
