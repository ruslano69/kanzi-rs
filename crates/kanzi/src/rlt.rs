// Port of kanzi-go's RLT (transform/RLT.go) -- an escaped run-length
// transform used as level 8/9's second stage (EXE+RLT+TEXT+UTF+DNA&TPAQ/X).
//
// Go's Forward reads two context hints: `ctx["dataType"]` (decline
// immediately if a prior stage already classified the data as DNA/BASE64/
// UTF8) and `ctx["entropy"]` (skip the "find best escape" histogram scan
// for fast entropy coders: NONE/ANS0/HUFFMAN/RANGE). This project only
// uses RLT at levels 8/9, whose entropy is always TPAQ/TPAQX -- never in
// that fast list -- so `findBestEscape` is unconditionally true here; only
// the `dataType` hint is threaded as a parameter.

use crate::datatype::{detect_simple_type, histogram, DataType};

const RUN_LEN_ENCODE1: i32 = 224;
const RUN_LEN_ENCODE2: i32 = (255 - RUN_LEN_ENCODE1) << 8;
const RUN_THRESHOLD: i32 = 3;
const MAX_RUN: i32 = 0xFFFF + RUN_LEN_ENCODE2 + RUN_THRESHOLD - 1;
const MAX_RUN4: i32 = MAX_RUN - 4;
const MIN_BLOCK_LENGTH: usize = 16;

pub fn max_encoded_len(src_len: usize) -> usize {
    if src_len <= 512 {
        src_len + 32
    } else {
        src_len
    }
}

fn emit_run_length(dst: &mut [u8], run: i32) -> usize {
    let mut run = run - RUN_THRESHOLD;

    if run < RUN_LEN_ENCODE1 {
        dst[0] = run as u8;
        return 1;
    }

    let dst_idx;

    if run < RUN_LEN_ENCODE2 {
        run -= RUN_LEN_ENCODE1;
        dst[0] = (RUN_LEN_ENCODE1 + (run >> 8)) as u8;
        dst_idx = 1;
    } else {
        run -= RUN_LEN_ENCODE2;
        dst[0] = 0xFF;
        dst[1] = (run >> 8) as u8;
        dst_idx = 2;
    }

    dst[dst_idx] = run as u8;
    dst_idx + 1
}

pub fn forward(src: &[u8], dst: &mut [u8], dt_in: DataType) -> Result<(usize, usize, DataType), (&'static str, DataType)> {
    if src.is_empty() || dst.is_empty() {
        return Ok((0, 0, dt_in));
    }

    if src.len() < MIN_BLOCK_LENGTH {
        return Err(("RLT forward transform skip: input buffer is too small", dt_in));
    }

    if dst.len() < max_encoded_len(src.len()) {
        return Err(("RLT forward transform skip: output buffer is too small", dt_in));
    }

    if dt_in == DataType::Dna || dt_in == DataType::Base64 || dt_in == DataType::Utf8 {
        return Err(("RLT forward transform skip", dt_in));
    }

    let freqs = histogram(src);
    let mut dt = dt_in;

    if dt == DataType::Undefined {
        dt = detect_simple_type(src.len(), &freqs);

        if dt == DataType::Dna || dt == DataType::Base64 || dt == DataType::Utf8 {
            return Err(("RLT forward transform skip", dt));
        }
    }

    let mut min_idx = 0usize;

    if freqs[0] > 0 {
        for i in 0..256usize {
            if freqs[i] < freqs[min_idx] {
                min_idx = i;

                if freqs[i] == 0 {
                    break;
                }
            }
        }
    }

    let escape = min_idx as u8;

    let src_end = src.len() as i32;
    let src_end4 = src_end - 4;
    let dst_end = dst.len() as i32;
    let mut run: i32 = 0;
    let mut src_idx: i32 = 0;
    let mut dst_idx: i32 = 0;
    let mut err: Option<&'static str> = None;
    let mut prev = src[src_idx as usize];
    src_idx += 1;
    dst[dst_idx as usize] = escape;
    dst_idx += 1;
    dst[dst_idx as usize] = prev;
    dst_idx += 1;

    if prev == escape {
        dst[dst_idx as usize] = 0;
        dst_idx += 1;
    }

    'main: loop {
        if prev == src[src_idx as usize] {
            let v = 0x0101_0101u32.wrapping_mul(prev as u32);
            let word = u32::from_le_bytes(src[src_idx as usize..src_idx as usize + 4].try_into().unwrap());

            if v == word {
                src_idx += 4;
                run += 4;

                if run < MAX_RUN4 && src_idx < src_end4 {
                    continue 'main;
                }
            } else {
                src_idx += 1;
                run += 1;

                if prev == src[src_idx as usize] {
                    src_idx += 1;
                    run += 1;

                    if prev == src[src_idx as usize] {
                        src_idx += 1;
                        run += 1;

                        if run < MAX_RUN4 && src_idx < src_end4 {
                            continue 'main;
                        }
                    }
                }
            }
        }

        if run > RUN_THRESHOLD {
            if dst_idx + 6 >= dst_end {
                err = Some("RLT forward transform skip: output buffer is too small");
                break 'main;
            }

            dst[dst_idx as usize] = prev;
            dst_idx += 1;

            if prev == escape {
                dst[dst_idx as usize] = 0;
                dst_idx += 1;
            }

            dst[dst_idx as usize] = escape;
            dst_idx += 1;
            dst_idx += emit_run_length(&mut dst[dst_idx as usize..dst_end as usize], run) as i32;
        } else if prev != escape {
            if dst_idx + run >= dst_end {
                err = Some("RLT forward transform skip: output buffer is too small");
                break 'main;
            }

            while run > 0 {
                dst[dst_idx as usize] = prev;
                dst_idx += 1;
                run -= 1;
            }
        } else {
            if dst_idx + 2 * run >= dst_end {
                err = Some("RLT forward transform skip: output buffer is too small");
                break 'main;
            }

            while run > 0 {
                dst[dst_idx as usize] = escape;
                dst[(dst_idx + 1) as usize] = 0;
                dst_idx += 2;
                run -= 1;
            }
        }

        prev = src[src_idx as usize];
        src_idx += 1;
        run = 1;

        if src_idx >= src_end4 {
            break 'main;
        }
    }

    if err.is_none() {
        // run == 1
        if prev != escape {
            if dst_idx + run < dst_end {
                while run > 0 {
                    dst[dst_idx as usize] = prev;
                    dst_idx += 1;
                    run -= 1;
                }
            }
        } else if dst_idx + 2 * run < dst_end {
            while run > 0 {
                dst[dst_idx as usize] = escape;
                dst[(dst_idx + 1) as usize] = 0;
                dst_idx += 2;
                run -= 1;
            }
        }

        // Emit the last few bytes.
        while src_idx < src_end && dst_idx < dst_end {
            if src[src_idx as usize] == escape {
                if dst_idx + 2 >= dst_end {
                    break;
                }

                dst[dst_idx as usize] = escape;
                dst[(dst_idx + 1) as usize] = 0;
                dst_idx += 2;
                src_idx += 1;
                continue;
            }

            dst[dst_idx as usize] = src[src_idx as usize];
            src_idx += 1;
            dst_idx += 1;
        }

        if src_idx != src_end {
            err = Some("RLT forward transform skip: output buffer is too small");
        } else if dst_idx >= src_idx {
            err = Some("RLT forward transform skip: no compression");
        }
    }

    match err {
        None => Ok((src_idx as usize, dst_idx as usize, dt)),
        Some(e) => Err((e, dt)),
    }
}

pub fn inverse(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
    if src.is_empty() || dst.is_empty() {
        return Ok((0, 0));
    }

    let src_end = src.len();
    let dst_end = dst.len();
    let escape = src[0];
    let mut src_idx = 1usize;
    let mut dst_idx = 0usize;

    if src_idx >= src_end {
        return Err("RLT inverse transform failed: invalid data");
    }

    if src[src_idx] == escape {
        src_idx += 1;

        // The data cannot start with a run but may start with an escape literal.
        if src_idx < src_end && src[src_idx] != 0 {
            return Err("RLT inverse transform failed: input starts with a run");
        }

        if src_idx >= src_end {
            return Err("RLT inverse transform failed: invalid data");
        }

        src_idx += 1;
        dst[dst_idx] = escape;
        dst_idx += 1;
    }

    let mut err: Option<&'static str> = None;

    while src_idx < src_end {
        if src[src_idx] != escape {
            if dst_idx >= dst_end {
                err = Some("RLT inverse transform failed: invalid data");
                break;
            }

            dst[dst_idx] = src[src_idx];
            src_idx += 1;
            dst_idx += 1;
            continue;
        }

        src_idx += 1;

        if src_idx >= src_end {
            err = Some("RLT inverse transform failed: invalid data");
            break;
        }

        let mut run = src[src_idx] as i32;
        src_idx += 1;

        if run == 0 {
            if dst_idx >= dst_end {
                err = Some("RLT inverse transform failed: invalid data");
                break;
            }

            dst[dst_idx] = escape;
            dst_idx += 1;
            continue;
        }

        if run == 0xFF {
            if src_idx + 1 >= src_end {
                err = Some("RLT inverse transform failed: invalid data");
                break;
            }

            run = ((src[src_idx] as i32) << 8) | src[src_idx + 1] as i32;
            src_idx += 2;
            run += RUN_LEN_ENCODE2;
        } else if run >= RUN_LEN_ENCODE1 {
            if src_idx >= src_end {
                err = Some("RLT inverse transform failed: invalid data");
                break;
            }

            run = ((run - RUN_LEN_ENCODE1) << 8) | src[src_idx] as i32;
            run += RUN_LEN_ENCODE1;
            src_idx += 1;
        }

        run += RUN_THRESHOLD - 1;

        if run > MAX_RUN || dst_idx as i32 + run > dst_end as i32 {
            err = Some("RLT inverse transform failed: invalid run length");
            break;
        }

        let val = dst[dst_idx - 1];
        let run = run as usize;

        for b in &mut dst[dst_idx..dst_idx + run] {
            *b = val;
        }

        dst_idx += run;
    }

    if err.is_none() && src_idx != src_end {
        err = Some("RLT inverse transform failed: invalid data");
    }

    match err {
        None => Ok((src_idx, dst_idx)),
        Some(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatype::DataType;

    fn roundtrip(data: &[u8]) {
        let mut dst = vec![0u8; max_encoded_len(data.len())];
        let (read, written, _dt) =
            forward(data, &mut dst, DataType::Undefined).expect("forward should not decline");
        assert_eq!(read, data.len());
        dst.truncate(written);

        let mut back = vec![0u8; data.len() + 16];
        let (_, back_len) = inverse(&dst, &mut back).expect("inverse should not fail");
        back.truncate(back_len);
        assert_eq!(back, data, "RLT round-trip mismatch");
    }

    #[test]
    fn regression_run_lengths_around_medium_form_threshold() {
        // emit_run_length()'s wire format switches from a 1-byte to a
        // 2-byte encoded length once the run (minus RUN_THRESHOLD) reaches
        // RUN_LEN_ENCODE1 (224), i.e. an actual run length around 227. A
        // shadowing bug there (`let run = ...` instead of reassigning)
        // corrupted every run using the 2-byte form; sweep both sides of
        // the boundary plus well past it (the 3-byte form) so any future
        // regression at either threshold fails here instead of needing a
        // 200MB real-world corpus to surface.
        for run_len in [1usize, 2, 16, 100, 224, 225, 226, 227, 228, 229, 230, 300, 1000, 8200, 100_000] {
            // escape byte (least-frequent, here just something absent from
            // the run) + the run itself + a differing tail so the run has
            // a clear end and MIN_BLOCK_LENGTH is comfortably exceeded.
            let mut data = vec![0xFFu8; 20];
            data.extend(vec![0x00u8; run_len]);
            data.extend(vec![0xFFu8; 20]);
            roundtrip(&data);
        }
    }

    #[test]
    fn roundtrip_run_at_very_end_of_buffer() {
        // The trailing-run edge case that originally surfaced the bug: a
        // long run of identical bytes ending exactly at the buffer's end
        // (no differing tail byte after it), which forces the run through
        // the main loop's boundary handling rather than the simple case.
        let mut data = vec![0xFFu8; 20];
        data.extend(vec![0x00u8; 230]);
        roundtrip(&data);
    }

    #[test]
    fn declines_input_below_min_block_length() {
        let data = vec![0u8; MIN_BLOCK_LENGTH - 1];
        let mut dst = vec![0u8; max_encoded_len(data.len())];
        assert!(forward(&data, &mut dst, DataType::Undefined).is_err());
    }

    #[test]
    fn inverse_rejects_truncated_input() {
        // escape(0x00) + escape(0x00) claims "literal escape byte" but is
        // missing the required trailing 0 marker byte -- truncated.
        let mut dst = vec![0u8; 16];
        assert!(inverse(&[0x00, 0x00], &mut dst).is_err());
    }
}
