// Port of kanzi-go's AliasCodec (transform/AliasCodec.go), used in two roles:
//   - DNA stage (onlyDNA=true, level 2: DNA+LZ) -- packs nucleotide digrams.
//   - PACK stage (onlyDNA=false, level 3+: TEXT+...+PACK+...) -- packs the
//     most frequent digrams of any data into unused single-byte aliases, or
//     bit-packs tiny alphabets (n0>=240).
//
// Both Forward and Inverse are fully ported, success path included (like
// utf.rs and fsd.rs, and unlike a plain decline-only stub): the success
// path is data-dependent and common (e.g. PACK applies to ordinary
// source-code text), so declining-only would be incorrect, not just
// suboptimal. The ctx["dataType"] side effect (set even when declining) is
// modeled as a DataType value threaded through Forward's Ok/Err returns,
// exactly like text_codec.rs does.

use crate::datatype::{detect_simple_type, histogram, DataType};

pub const ALIAS_MIN_BLOCKSIZE: usize = 1024;

pub fn max_encoded_len(src_len: usize) -> usize {
    src_len + 1024
}

/// AliasCodec.Forward. `dt_in` is the pipeline's current data type
/// (Undefined for the first stage); `only_dna` selects the DNA (level 2) vs
/// PACK (level 3+) role. Returns (bytes_read, bytes_written, data_type) --
/// the data type accompanies both Ok and Err, mirroring Go's ctx["dataType"]
/// write that happens even when the transform declines.
pub fn forward(
    src: &[u8],
    dst: &mut [u8],
    dt_in: DataType,
    only_dna: bool,
) -> Result<(usize, usize, DataType), (&'static str, DataType)> {
    let count = src.len();

    if count == 0 || dst.is_empty() {
        return Ok((0, 0, dt_in));
    }

    if dst.len() < max_encoded_len(count) {
        return Err(("Output buffer is too small", dt_in));
    }

    if count < ALIAS_MIN_BLOCKSIZE {
        return Err(("Input block is too small", dt_in));
    }

    let mut dt = dt_in;

    if dt == DataType::Multimedia || dt == DataType::Utf8 {
        return Err(("Alias Codec: forward transform skip, binary data", dt));
    }

    if dt == DataType::Exe || dt == DataType::Bin {
        return Err(("Alias Codec: forward transform skip, binary data", dt));
    }

    if only_dna && dt != DataType::Undefined && dt != DataType::Dna {
        return Err(("DNA Alias Codec: forward transform skip, not DNA data", dt));
    }

    // Find missing 1-byte symbols
    let freqs0 = histogram(src);
    let mut absent = [0usize; 256];
    let mut n0 = 0usize;

    for (i, &f) in freqs0.iter().enumerate() {
        if f == 0 {
            absent[n0] = i;
            n0 += 1;
        }
    }

    if n0 < 16 {
        return Err((
            "Alias Codec: forward transform skip, not enough free slots",
            dt,
        ));
    }

    if dt == DataType::Undefined {
        dt = detect_simple_type(count, &freqs0);

        if dt != DataType::Dna && only_dna {
            return Err(("DNA Alias Codec: forward transform skip, not DNA data", dt));
        }
    }

    let (src_idx, dst_idx);

    if n0 >= 240 {
        // Small alphabet => pack bits
        dst[0] = n0 as u8;

        if n0 == 255 {
            // One symbol
            dst[1] = src[0];
            dst[2..6].copy_from_slice(&(count as u32).to_le_bytes());
            src_idx = count;
            dst_idx = 6;
        } else {
            let mut map8 = [0u8; 256];
            let mut s_idx = 0usize;
            let mut d_idx = 1usize;
            let mut j = 0u8;

            for (i, &f) in freqs0.iter().enumerate() {
                if f != 0 {
                    dst[d_idx] = i as u8;
                    d_idx += 1;
                    map8[i] = j;
                    j += 1;
                }
            }

            if n0 >= 252 {
                // 4 symbols or less
                let c3 = count & 3;
                dst[d_idx] = c3 as u8;
                d_idx += 1;
                dst[d_idx..d_idx + c3].copy_from_slice(&src[s_idx..s_idx + c3]);
                s_idx += c3;
                d_idx += c3;

                while s_idx < count {
                    dst[d_idx] = (map8[src[s_idx] as usize] << 6)
                        | (map8[src[s_idx + 1] as usize] << 4)
                        | (map8[src[s_idx + 2] as usize] << 2)
                        | map8[src[s_idx + 3] as usize];
                    s_idx += 4;
                    d_idx += 1;
                }
            } else {
                // 16 symbols or less
                dst[d_idx] = (count & 1) as u8;
                d_idx += 1;

                if (count & 1) != 0 {
                    dst[d_idx] = src[s_idx];
                    s_idx += 1;
                    d_idx += 1;
                }

                while s_idx < count {
                    dst[d_idx] = (map8[src[s_idx] as usize] << 4) | map8[src[s_idx + 1] as usize];
                    s_idx += 2;
                    d_idx += 1;
                }
            }

            src_idx = s_idx;
            dst_idx = d_idx;
        }
    } else {
        // Digram encoding
        let mut freqs1 = vec![0i64; 65536];
        let mut prv = 0usize;

        for &b in src {
            freqs1[prv * 256 + b as usize] += 1;
            prv = b as usize;
        }

        // (symbol, frequency) for every distinct digram. Go sorts a fixed
        // [65536]sdAlias array; a dense Vec of the non-zero entries sorts
        // identically (the comparator is a total order, so stability is
        // irrelevant) without a 1MB stack array.
        let mut symb: Vec<(u32, i64)> = freqs1
            .iter()
            .enumerate()
            .filter(|&(_, &f)| f != 0)
            .map(|(i, &f)| (i as u32, f))
            .collect();
        let n1 = symb.len();

        if n0 > n1 {
            // Fewer distinct 2-byte symbols than free 1-byte slots
            n0 = n1;

            if n0 < 16 {
                return Err((
                    "Alias Codec: forward transform skip, not enough free slots",
                    dt,
                ));
            }
        }

        // Sort by decreasing frequency, ties by decreasing symbol value
        // (matches slices.SortStableFunc(sd2.freq-sd1.freq, sd2.val-sd1.val)).
        symb.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));

        // Build map symbol -> alias (0x100|hi = literal 1-byte step,
        // 0x200|alias = packed 2-byte step).
        let mut map16 = vec![0i32; 65536];

        for (i, m) in map16.iter_mut().enumerate() {
            *m = 0x100 | ((i >> 8) as i32);
        }

        let mut savings: i64 = 0;
        dst[0] = n0 as u8;
        dst[1] = 0;
        let mut s_idx = 0usize;
        let mut d_idx = 2usize;

        // Header: emit map length then map data
        for i in 0..n0 {
            savings += symb[i].1; // ignore factor 2
            let idx = symb[i].0 as usize;
            map16[idx] = 0x200 | absent[i] as i32;
            dst[d_idx] = (idx >> 8) as u8;
            dst[d_idx + 1] = (idx & 0xFF) as u8;
            dst[d_idx + 2] = absent[i] as u8;
            d_idx += 3;
        }

        // Worth it ?
        if savings < (count / 20) as i64 {
            return Err((
                "Alias Codec: forward transform skip, not enough savings",
                dt,
            ));
        }

        let src_end = count - 1;

        // Emit aliased data
        while s_idx < src_end {
            let alias = map16[(src[s_idx] as usize) << 8 | src[s_idx + 1] as usize];
            dst[d_idx] = (alias & 0xFF) as u8;
            s_idx += (alias >> 8) as usize;
            d_idx += 1;
        }

        if s_idx != count {
            dst[1] = 1;
            dst[d_idx] = src[s_idx];
            s_idx += 1;
            d_idx += 1;
        }

        src_idx = s_idx;
        dst_idx = d_idx;
    }

    if dst_idx >= count {
        return Err((
            "Alias Codec: forward transform skip, not enough savings",
            dt,
        ));
    }

    Ok((src_idx, dst_idx, dt))
}

/// AliasCodec.Inverse. Inverse of either role (DNA or PACK share the wire
/// format); `src` is the entropy-decoded post-transform buffer.
pub fn inverse(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
    if src.is_empty() || dst.is_empty() {
        return Ok((0, 0));
    }

    if src.len() < 2 {
        return Err("Input block is too small");
    }

    let mut n = src[0] as usize;

    if n < 16 {
        return Err(
            "Alias codec inverse transform failed: invalid data (incorrect number of slots)",
        );
    }

    let (src_idx, dst_idx);

    if n >= 240 {
        let src_end = src.len();
        n = 256 - n;
        let mut s_idx = 1usize;
        let mut d_idx = 0usize;

        if n == 1 {
            // One symbol
            if src.len() < 6 {
                return Err("Alias codec inverse transform failed: truncated header");
            }

            let val = src[1];
            let o_size = u32::from_le_bytes(src[2..6].try_into().unwrap()) as usize;

            if o_size > dst.len() {
                return Err(
                    "Alias codec inverse transform failed: invalid data (incorrect output size)",
                );
            }

            for b in dst[0..o_size].iter_mut() {
                *b = val;
            }

            s_idx = src_end;
            d_idx = o_size;
        } else {
            // Rebuild map alias -> symbol
            if src.len() < s_idx + n + 1 {
                return Err("Alias codec inverse transform failed: truncated header");
            }

            let mut idx2symb = [0u8; 16];

            for i in 0..n {
                idx2symb[i] = src[s_idx];
                s_idx += 1;
            }

            let adjust = src[s_idx] as usize;
            s_idx += 1;

            if adjust > 3 {
                return Err("Alias codec inverse transform failed: invalid data");
            }

            if n <= 4 {
                if adjust > src_end - s_idx
                    || adjust > dst.len() - d_idx
                    || src_end - s_idx - adjust > (dst.len() - d_idx - adjust) / 4
                {
                    return Err("Alias codec inverse transform failed: invalid data");
                }

                // 4 symbols or less
                let mut decode_map = [0u32; 256];

                for (i, dm) in decode_map.iter_mut().enumerate() {
                    let mut val = idx2symb[(i >> 0) & 0x03] as u32;
                    val <<= 8;
                    val |= idx2symb[(i >> 2) & 0x03] as u32;
                    val <<= 8;
                    val |= idx2symb[(i >> 4) & 0x03] as u32;
                    val <<= 8;
                    val |= idx2symb[(i >> 6) & 0x03] as u32;
                    *dm = val;
                }

                dst[d_idx..d_idx + adjust].copy_from_slice(&src[s_idx..s_idx + adjust]);
                s_idx += adjust;
                d_idx += adjust;

                while s_idx < src_end {
                    dst[d_idx..d_idx + 4]
                        .copy_from_slice(&decode_map[src[s_idx] as usize].to_le_bytes());
                    s_idx += 1;
                    d_idx += 4;
                }
            } else {
                if adjust > src_end - s_idx
                    || adjust > dst.len() - d_idx
                    || src_end - s_idx - adjust > (dst.len() - d_idx - adjust) / 2
                {
                    return Err("Alias codec inverse transform failed: invalid data");
                }

                // 16 symbols or less
                let mut decode_map = [0u16; 256];

                for (i, dm) in decode_map.iter_mut().enumerate() {
                    let mut val = idx2symb[i & 0x0F] as u16;
                    val <<= 8;
                    val |= idx2symb[i >> 4] as u16;
                    *dm = val;
                }

                if adjust != 0 {
                    dst[d_idx] = src[s_idx];
                    s_idx += 1;
                    d_idx += 1;
                }

                while s_idx < src_end {
                    let val = decode_map[src[s_idx] as usize];
                    s_idx += 1;
                    dst[d_idx..d_idx + 2].copy_from_slice(&val.to_le_bytes());
                    d_idx += 2;
                }
            }
        }

        src_idx = s_idx;
        dst_idx = d_idx;
    } else {
        // Rebuild map alias -> symbol
        let adjust = src[1] as usize;

        if adjust > 1 || src.len() < 2 + 3 * n + adjust {
            return Err("Alias codec inverse transform failed: truncated header");
        }

        let src_end = src.len() - adjust;
        let mut s_idx = 2usize;
        let mut d_idx = 0usize;
        let mut map16 = [0i32; 256];

        for (i, m) in map16.iter_mut().enumerate() {
            *m = 0x10000 | i as i32;
        }

        for _ in 0..n {
            map16[src[s_idx + 2] as usize] =
                0x20000 | src[s_idx] as i32 | ((src[s_idx + 1] as i32) << 8);
            s_idx += 3;
        }

        let nb_aliases = src_end - s_idx;
        let dst_avail = dst.len() - d_idx;

        if nb_aliases <= (dst_avail >> 1) {
            while s_idx < src_end {
                let val = map16[src[s_idx] as usize];
                s_idx += 1;
                dst[d_idx] = (val & 0xFF) as u8;
                dst[d_idx + 1] = ((val >> 8) & 0xFF) as u8;
                d_idx += (val >> 16) as usize;
            }
        } else {
            while s_idx < src_end && d_idx + 1 < dst.len() {
                let val = map16[src[s_idx] as usize];
                s_idx += 1;
                dst[d_idx] = (val & 0xFF) as u8;
                dst[d_idx + 1] = ((val >> 8) & 0xFF) as u8;
                d_idx += (val >> 16) as usize;
            }

            while s_idx < src_end {
                let val = map16[src[s_idx] as usize];
                s_idx += 1;
                let inc = (val >> 16) as usize;

                if d_idx + inc > dst.len() {
                    return Err("Alias codec inverse transform failed: invalid data");
                }

                dst[d_idx + inc - 1] = ((val >> 8) & 0xFF) as u8;
                dst[d_idx] = (val & 0xFF) as u8;
                d_idx += inc;
            }
        }

        if adjust != 0 {
            if d_idx >= dst.len() {
                return Err("Alias codec inverse transform failed: invalid data");
            }

            dst[d_idx] = src[s_idx];
            s_idx += 1;
            d_idx += 1;
        }

        src_idx = s_idx;
        dst_idx = d_idx;
    }

    Ok((src_idx, dst_idx))
}
