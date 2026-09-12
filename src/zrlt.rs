// Port of kanzi-go's ZRLT (transform/ZRLT.go) -- Zero Run Length Transform
// for post-BWT/RANK data: zero runs become binary-length bit bytes (MSB
// implied), non-zero values shift by +1, values >= 0xFE escape via 0xFF.
// Both Forward and Inverse are fully ported, including the SWAR fast path
// in the inverse (lane-wise "below 2 or equal 0xFF" detection).

pub fn max_encoded_len(src_len: usize) -> usize {
    src_len
}

fn log2_no_check(x: u32) -> u32 {
    31 - x.leading_zeros()
}

pub fn forward(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
    let count = src.len();

    if count == 0 || dst.is_empty() {
        return Ok((0, 0));
    }

    if dst.len() < max_encoded_len(count) {
        return Err("Output buffer is too small");
    }

    let src_end = count;
    let dst_end = count; // do not expand, hence len(src)
    let mut src_idx = 0usize;
    let mut dst_idx = 0usize;
    let mut ok = true;

    while src_idx < src_end {
        if src[src_idx] == 0 {
            // Go uses wrapping uint arithmetic here: a run at position 0
            // makes run_start wrap, and run_length still comes out as
            // zeros+1. Replicated with explicit wrapping_*.
            let run_start = src_idx.wrapping_sub(1);
            src_idx += 1;

            while src_idx + 1 < src_end && (src[src_idx] | src[src_idx + 1]) == 0 {
                src_idx += 2;
            }

            while src_idx < src_end && src[src_idx] == 0 {
                src_idx += 1;
            }

            // Encode length
            let run_length = src_idx.wrapping_sub(run_start);
            let log2 = log2_no_check(run_length as u32);

            // Go: dstIdx >= dstEnd-log2 (wrapping uint); saturating_sub
            // reaches the same verdict without panicking.
            if dst_idx >= dst_end.saturating_sub(log2 as usize) {
                ok = false;
                break;
            }

            // Write every bit as a byte except the most significant one
            let mut l = log2;

            while l > 0 {
                l -= 1;
                dst[dst_idx] = ((run_length >> l) & 1) as u8;
                dst_idx += 1;
            }

            continue;
        }

        if src[src_idx] >= 0xFE {
            if dst_idx >= dst_end - 1 {
                ok = false;
                break;
            }

            dst[dst_idx] = 0xFF;
            dst_idx += 1;
            dst[dst_idx] = src[src_idx] - 0xFE;
        } else {
            if dst_idx >= dst_end {
                ok = false;
                break;
            }

            dst[dst_idx] = src[src_idx] + 1;
        }

        src_idx += 1;
        dst_idx += 1;
    }

    if src_idx != src_end || !ok {
        return Err("ZRLT forward transform failed: output buffer is too small");
    }

    // NOTE: Go returns (srcIdx, dstIdx, err) with err set above; a failed
    // (expanding) block is a decline. Our Result carries the same meaning;
    // lengths are irrelevant on Err (callers discard dst), matching Go's
    // Sequence which restores the pre-stage length on error.
    Ok((src_idx, dst_idx))
}

pub fn inverse(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
    let count = src.len();

    if count == 0 || dst.is_empty() {
        return Ok((0, 0));
    }

    let (src_end, dst_end) = (src.len(), dst.len());
    let mut src_idx = 0usize;
    let mut dst_idx = 0usize;
    let mut run_length = 0usize;

    loop {
        if src[src_idx] <= 1 {
            // Generate the run length bit by bit (but force MSB)
            run_length = 1;

            while src[src_idx] <= 1 {
                run_length += run_length + src[src_idx] as usize;
                src_idx += 1;

                if src_idx >= src_end {
                    return finish_tail(run_length, src_idx, src_end, dst, dst_idx, dst_end);
                }
            }

            run_length -= 1;

            if run_length >= dst_end - dst_idx {
                break;
            }

            while run_length > 0 {
                run_length -= 1;
                dst[dst_idx] = 0;
                dst_idx += 1;
            }
        }

        // Regular data processing
        if src[src_idx] != 0xFF {
            let start_idx = src_idx;

            while src_idx + 4 <= src_end && dst_idx + 4 <= dst_end {
                let word = u32::from_le_bytes(src[src_idx..src_idx + 4].try_into().unwrap());
                // Detect bytes below 2 or equal to 0xFF before lane-wise
                // subtraction. Wrapping arithmetic like Go's uint32. NOTE:
                // method calls bind tighter than prefix `!`, so the second
                // lane must parenthesize (!word) explicitly -- Go's
                // `(^word - c)` is `((!word).wrapping_sub(c))`, NOT
                // `!(word.wrapping_sub(c))`.
                let invalid = (((word.wrapping_sub(0x0202_0202)) & !word)
                    | (((!word).wrapping_sub(0x0101_0101)) & word))
                    & 0x8080_8080;

                if invalid != 0 {
                    break;
                }

                dst[dst_idx..dst_idx + 4]
                    .copy_from_slice(&(word.wrapping_sub(0x0101_0101)).to_le_bytes());
                src_idx += 4;
                dst_idx += 4;
            }

            if src_idx != start_idx {
                if src_idx >= src_end || dst_idx >= dst_end {
                    break;
                }

                continue;
            }
        }

        if src[src_idx] == 0xFF {
            src_idx += 1;

            if src_idx >= src_end {
                break;
            }

            dst[dst_idx] = 0xFEu8.wrapping_add(src[src_idx]);
        } else {
            dst[dst_idx] = src[src_idx].wrapping_sub(1);
        }

        src_idx += 1;
        dst_idx += 1;

        if src_idx >= src_end || dst_idx >= dst_end {
            break;
        }
    }

    finish_tail(run_length, src_idx, src_end, dst, dst_idx, dst_end)
}

/// Shared tail of Go's inverse: flush a pending run length, then require
/// full consumption (mirrors the `End:` label and final checks).
fn finish_tail(
    mut run_length: usize,
    src_idx: usize,
    src_end: usize,
    dst: &mut [u8],
    mut dst_idx: usize,
    dst_end: usize,
) -> Result<(usize, usize), &'static str> {
    if run_length > 0 {
        run_length -= 1;

        // If runLength is not 1, add trailing 0s
        if run_length > dst_end - dst_idx {
            return Err("ZRLT inverse transform failed: output buffer too small");
        } else {
            while run_length > 0 {
                run_length -= 1;
                dst[dst_idx] = 0;
                dst_idx += 1;
            }
        }
    }

    if src_idx < src_end {
        return Err("ZRLT inverse transform failed: output buffer too small");
    }

    Ok((src_idx, dst_idx))
}
