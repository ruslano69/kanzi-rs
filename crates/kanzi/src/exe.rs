// Port of kanzi-go's EXECodec (transform/EXECodec.go) -- rewrites relative
// x86 CALL/JMP (E8/E9, 0F 8x) and ARM64 B/BL target addresses into absolute
// form so later stages compress them better. Both Forward and Inverse are
// fully ported (x86 and ARM64 variants).
//
// Fidelity notes:
// - Only the current (non-V2) wire format is ported: this project is v7-only
//   and Go selects inverseV2 solely for bitstream versions < 3.
// - The conditional-branch (CBZ/CBNZ) transform arm is disabled upstream
//   ("disable for now"): detection still counts those opcodes but the
//   transform treats every non-B/BL word as a literal -- replicated exactly.
// - Go's `copy()` truncates silently on size mismatch while Rust panics;
//   every copy site below is guarded by the same checks that make the sizes
//   match on valid streams, so a panic there means corrupt input (loud,
//   like Go's explicit validations elsewhere).

use crate::datatype::{detect_simple_type, DataType};
use crate::magic;

const EXE_X86_MASK_JUMP: u8 = 0xFE;
const EXE_X86_INSTRUCTION_JUMP: u8 = 0xE8;
const EXE_X86_INSTRUCTION_JCC: u8 = 0x80;
const EXE_X86_TWO_BYTE_PREFIX: u8 = 0x0F;
const EXE_X86_MASK_JCC: u8 = 0xF0;
const EXE_X86_ESCAPE: u8 = 0x9B;
const EXE_NOT_EXE: u8 = 0x80;
const EXE_X86: u8 = 0x40;
const EXE_ARM64: u8 = 0x20;
const EXE_MASK_DT: u8 = 0x0F;
const EXE_X86_ADDR_MASK: i64 = (1 << 24) - 1;
const EXE_MASK_ADDRESS: u32 = 0xF0F0_F0F0;
const EXE_ARM_B_ADDR_MASK: u32 = (1 << 26) - 1;
const EXE_ARM_B_OPCODE_MASK: u32 = 0xFFFF_FFFF ^ ((1 << 26) - 1);
const EXE_ARM_B_ADDR_SGN_MASK: u32 = 1 << 25;
const EXE_ARM_OPCODE_B: u32 = 0x1400_0000;
const EXE_ARM_OPCODE_BL: u32 = 0x9400_0000;
// NOTE: the CBZ/CBNZ *transform* arm is disabled upstream, so only the
// detection opcodes below are ported (the CB_ADDR_* layout constants have no
// live use here, same as Go's unreachable else branch).
const EXE_ARM_CB_OPCODE_MASK: u32 = 0x7F00_0000;
const EXE_ARM_OPCODE_CBZ: u32 = 0x3400_0000;
const EXE_ARM_OPCODE_CBNZ: u32 = 0x3500_0000;
const EXE_WIN_PE: u32 = 0x0000_4550;
const EXE_WIN_X86_ARCH: u16 = 0x014C;
const EXE_WIN_AMD64_ARCH: u16 = 0x8664;
const EXE_WIN_ARM64_ARCH: u16 = 0xAA64;
const EXE_ELF_X86_ARCH: u16 = 0x03;
const EXE_ELF_AMD64_ARCH: u16 = 0x3E;
const EXE_ELF_ARM64_ARCH: u16 = 0xB7;
const EXE_MAC_AMD64_ARCH: u32 = 0x0100_0007;
const EXE_MAC_ARM64_ARCH: u32 = 0x0100_000C;
const EXE_MAC_MH_EXECUTE: u32 = 0x02;
const EXE_MAC_LC_SEGMENT: u32 = 0x01;
const EXE_MAC_LC_SEGMENT64: u32 = 0x19;
const EXE_MIN_BLOCK_SIZE: usize = 4096;
const EXE_MAX_BLOCK_SIZE: usize = (1 << (26 + 2)) - 1;

pub fn max_encoded_len(src_len: usize) -> usize {
    // Allocate some extra buffer for incompressible data.
    if src_len <= 256 {
        src_len + 32
    } else {
        src_len + src_len / 8
    }
}

fn dt_from_code(code: u8) -> DataType {
    match code {
        1 => DataType::Text,
        2 => DataType::Multimedia,
        3 => DataType::Exe,
        4 => DataType::Numeric,
        5 => DataType::Base64,
        6 => DataType::Dna,
        7 => DataType::Bin,
        8 => DataType::Utf8,
        9 => DataType::SmallAlphabet,
        _ => DataType::Undefined,
    }
}

/// EXECodec.Forward. `dt_in` is the pipeline's current data type. Returns
/// (bytes_read, bytes_written, data_type) on both paths (Go's
/// ctx["dataType"] side effect: DT_EXE on success, detected type when the
/// input is rejected as non-executable).
pub fn forward(
    src: &[u8],
    dst: &mut [u8],
    dt_in: DataType,
) -> Result<(usize, usize, DataType), (&'static str, DataType)> {
    let count = src.len();

    if count == 0 || dst.is_empty() {
        return Ok((0, 0, dt_in));
    }

    if count < EXE_MIN_BLOCK_SIZE {
        return Err(("ExeCodec forward failed: Block too small", dt_in));
    }

    if count > EXE_MAX_BLOCK_SIZE {
        return Err(("ExeCodec forward failed: Block too big", dt_in));
    }

    if dst.len() < max_encoded_len(count) {
        return Err((
            "ExeCodec forward transform skip: Output buffer too small",
            dt_in,
        ));
    }

    if dt_in != DataType::Undefined && dt_in != DataType::Exe && dt_in != DataType::Bin {
        return Err((
            "ExeCodec forward transform skip: Input is not an executable",
            dt_in,
        ));
    }

    let (mut mode, code_start, code_end) = detect_exe_type(src);

    if mode & EXE_NOT_EXE != 0 {
        return Err((
            "ExeCodec forward transform skip: Input is not an executable",
            dt_from_code(mode & EXE_MASK_DT),
        ));
    }

    mode &= !EXE_MASK_DT;

    // Go writes ctx["dataType"] = DT_EXE only on success; a transform error
    // leaves the incoming type untouched (decline carries dt_in, not Exe).
    if mode == EXE_X86 {
        match forward_x86(src, dst, code_start, code_end) {
            Ok((r, w)) => Ok((r, w, DataType::Exe)),
            Err(e) => Err((e, dt_in)),
        }
    } else if mode == EXE_ARM64 {
        match forward_arm(src, dst, code_start, code_end) {
            Ok((r, w)) => Ok((r, w, DataType::Exe)),
            Err(e) => Err((e, dt_in)),
        }
    } else {
        Err((
            "ExeCodec forward transform skip: Input is not a supported executable format",
            DataType::Undefined,
        ))
    }
}

#[allow(clippy::too_many_lines)]
fn forward_x86(
    src: &[u8],
    dst: &mut [u8],
    code_start: usize,
    code_end: usize,
) -> Result<(usize, usize), &'static str> {
    let mut src_idx = code_start;
    let mut dst_idx = 9usize;
    let mut matches = 0usize;
    let dst_end = dst.len() - 5;
    dst[0] = EXE_X86;
    let mut boundary_reached = false;

    if code_end < code_start || code_end > src.len() {
        return Err("ExeCodec forward transform skip: Input is not a supported executable format");
    }

    if code_start > 0 {
        dst[dst_idx..dst_idx + code_start].copy_from_slice(&src[0..code_start]);
        dst_idx += code_start;
    }

    while src_idx < code_end && dst_idx < dst_end {
        if src[src_idx] == EXE_X86_TWO_BYTE_PREFIX {
            if src_idx + 1 >= code_end {
                boundary_reached = true;
                break;
            }

            if (src[src_idx + 1] & EXE_X86_MASK_JCC) == EXE_X86_INSTRUCTION_JCC
                && src_idx + 5 >= code_end
            {
                boundary_reached = true;
                break;
            }

            dst[dst_idx] = src[src_idx];
            src_idx += 1;
            dst_idx += 1;

            if (src[src_idx] & EXE_X86_MASK_JCC) != EXE_X86_INSTRUCTION_JCC {
                // Not a relative jump
                if src[src_idx] == EXE_X86_ESCAPE {
                    dst[dst_idx] = EXE_X86_ESCAPE;
                    dst_idx += 1;
                }

                dst[dst_idx] = src[src_idx];
                src_idx += 1;
                dst_idx += 1;
                continue;
            }

            if src_idx + 4 >= code_end {
                boundary_reached = true;
                break;
            }
        } else if (src[src_idx] & EXE_X86_MASK_JUMP) != EXE_X86_INSTRUCTION_JUMP {
            // Not a relative call
            if src[src_idx] == EXE_X86_ESCAPE {
                dst[dst_idx] = EXE_X86_ESCAPE;
                dst_idx += 1;
            }

            dst[dst_idx] = src[src_idx];
            src_idx += 1;
            dst_idx += 1;
            continue;
        } else if src_idx + 4 >= code_end {
            boundary_reached = true;
            break;
        }

        // Current instruction is a jump/call.
        let sgn = src[src_idx + 4];
        let offset = u32::from_le_bytes(src[src_idx + 1..src_idx + 5].try_into().unwrap());

        if (sgn != 0 && sgn != 0xFF) || (offset == 0xFF00_0000) {
            dst[dst_idx] = EXE_X86_ESCAPE;
            dst[dst_idx + 1] = src[src_idx];
            src_idx += 1;
            dst_idx += 2;
            continue;
        }

        // Absolute target address = srcIdx + 5 + offset. Let us ignore the +5
        let mut addr = src_idx as i64;

        if sgn == 0 {
            addr += offset as i64;
        } else {
            addr -= -(offset as i64) & EXE_X86_ADDR_MASK;
        }

        dst[dst_idx] = src[src_idx];
        dst[dst_idx + 1..dst_idx + 5]
            .copy_from_slice(&((addr ^ EXE_MASK_ADDRESS as i64) as u32).to_be_bytes());
        src_idx += 5;
        dst_idx += 5;
        matches += 1;
    }

    if matches < 16 {
        return Err("ExeCodec forward transform skip: Too few calls/jumps");
    }

    let count = src.len();

    // Cap expansion due to false positives
    if (src_idx < code_end) && !boundary_reached {
        return Err("ExeCodec forward transform skip: Too many false positives");
    }

    if dst_idx + (count - src_idx) > dst_end {
        return Err("ExeCodec forward transform skip: Too many false positives");
    }

    dst[1..5].copy_from_slice(&(code_start as u32).to_le_bytes());
    dst[5..9].copy_from_slice(&(dst_idx as u32).to_le_bytes());
    dst[dst_idx..dst_idx + (count - src_idx)].copy_from_slice(&src[src_idx..count]);
    dst_idx += count - src_idx;

    // Cap expansion due to false positives
    if dst_idx > count + (count / 50) {
        return Err("ExeCodec forward transform skip: Too many false positives");
    }

    Ok((count, dst_idx))
}

/// EXECodec inverse (current wire format; v7-only, see module note).
pub fn inverse(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
    if src.is_empty() || dst.is_empty() {
        return Ok((0, 0));
    }

    if src.len() < 9 {
        return Err("ExeCodec inverse transform failed: invalid data");
    }

    let mode = src[0];

    if mode == EXE_X86 {
        return inverse_x86(src, dst);
    }

    if mode == EXE_ARM64 {
        return inverse_arm(src, dst);
    }

    Err("ExeCodec inverse transform failed: unknown binary type")
}

fn inverse_x86(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
    let mut src_idx = 9usize;
    let mut dst_idx = 0usize;
    let code_start = u32::from_le_bytes(src[1..5].try_into().unwrap()) as usize;
    let code_end = u32::from_le_bytes(src[5..9].try_into().unwrap()) as usize;

    // Sanity check
    if code_end < src_idx
        || code_end > src.len()
        || code_start > code_end - src_idx
        || code_start > dst.len() - dst_idx
    {
        return Err("ExeCodec inverse transform failed: invalid data");
    }

    if code_start > 0 {
        dst[dst_idx..dst_idx + code_start].copy_from_slice(&src[src_idx..src_idx + code_start]);
        dst_idx += code_start;
        src_idx += code_start;
    }

    while src_idx < code_end {
        if src[src_idx] == EXE_X86_TWO_BYTE_PREFIX {
            if src_idx + 1 >= code_end {
                // Accept legacy streams where a trailing 0x0F was emitted in
                // the code section and the remaining bytes were copied as tail.
                if dst_idx >= dst.len() {
                    return Err("ExeCodec inverse transform failed: invalid data");
                }

                dst[dst_idx] = src[src_idx];
                src_idx += 1;
                dst_idx += 1;
                break;
            }

            if dst_idx >= dst.len() {
                return Err("ExeCodec inverse transform failed: invalid data");
            }

            dst[dst_idx] = src[src_idx];
            src_idx += 1;
            dst_idx += 1;

            if (src[src_idx] & EXE_X86_MASK_JCC) != EXE_X86_INSTRUCTION_JCC {
                // Not a relative jump
                if src[src_idx] == EXE_X86_ESCAPE {
                    src_idx += 1;

                    if src_idx >= code_end {
                        return Err("ExeCodec inverse transform failed: invalid data");
                    }
                }

                if dst_idx >= dst.len() {
                    return Err("ExeCodec inverse transform failed: invalid data");
                }

                dst[dst_idx] = src[src_idx];
                src_idx += 1;
                dst_idx += 1;
                continue;
            }
        } else if (src[src_idx] & EXE_X86_MASK_JUMP) != EXE_X86_INSTRUCTION_JUMP {
            // Not a relative call
            if src[src_idx] == EXE_X86_ESCAPE {
                src_idx += 1;

                if src_idx >= code_end {
                    return Err("ExeCodec inverse transform failed: invalid data");
                }
            }

            if dst_idx >= dst.len() {
                return Err("ExeCodec inverse transform failed: invalid data");
            }

            dst[dst_idx] = src[src_idx];
            src_idx += 1;
            dst_idx += 1;
            continue;
        }

        if src_idx + 4 >= code_end {
            return Err("ExeCodec inverse transform failed: invalid data");
        }

        if dst_idx + 5 > dst.len() {
            return Err("ExeCodec inverse transform failed: invalid data");
        }

        // Current instruction is a jump/call. Decode absolute address
        let addr = (u32::from_be_bytes(src[src_idx + 1..src_idx + 5].try_into().unwrap())
            ^ EXE_MASK_ADDRESS) as i32 as i64;
        let offset = addr - dst_idx as i64;
        // Go: int32(offset) or -int32((-offset) & mask), wrapping like all
        // Go int arithmetic.
        let encoded_offset = if offset >= 0 {
            offset as i32
        } else {
            (((-offset) & EXE_X86_ADDR_MASK) as i32).wrapping_neg()
        };

        dst[dst_idx] = src[src_idx];
        src_idx += 1;
        dst_idx += 1;

        dst[dst_idx..dst_idx + 4].copy_from_slice(&encoded_offset.to_le_bytes());

        src_idx += 4;
        dst_idx += 4;
    }

    let count = src.len();

    if dst_idx + (count - src_idx) > dst.len() {
        return Err("ExeCodec inverse transform failed: invalid data");
    }

    if src_idx < count {
        dst[dst_idx..dst_idx + (count - src_idx)].copy_from_slice(&src[src_idx..count]);
        dst_idx += count - src_idx;
    }

    Ok((count, dst_idx))
}

fn forward_arm(
    src: &[u8],
    dst: &mut [u8],
    code_start: usize,
    code_end: usize,
) -> Result<(usize, usize), &'static str> {
    let mut src_idx = code_start;
    let mut dst_idx = 9usize;
    let mut matches = 0usize;
    let dst_end = dst.len() - 8;
    dst[0] = EXE_ARM64;

    if code_end < code_start || code_end > src.len() {
        return Err("ExeCodec forward failed: Input is not a supported executable format");
    }

    if code_start > 0 {
        dst[dst_idx..dst_idx + code_start].copy_from_slice(&src[0..code_start]);
        dst_idx += code_start;
    }

    while src_idx + 4 <= code_end && dst_idx < dst_end {
        let instr = u32::from_le_bytes(src[src_idx..src_idx + 4].try_into().unwrap());
        let opcode1 = instr & EXE_ARM_B_OPCODE_MASK;
        // CBZ/CBNZ transform arm disabled upstream ("disable for now"):
        // every non-B/BL word is a literal, like Go.
        let is_bl = (opcode1 == EXE_ARM_OPCODE_B) || (opcode1 == EXE_ARM_OPCODE_BL);

        if !is_bl {
            // Not a relative jump
            dst[dst_idx..dst_idx + 4].copy_from_slice(&src[src_idx..src_idx + 4]);
            src_idx += 4;
            dst_idx += 4;
            continue;
        }

        // opcode(6) + sgn(1) + offset(25)
        // Absolute target address = srcIdx +/- (offset*4).
        // Go: int(int32(...)) sign-extends the 26-bit field; the negative
        // arm is srcIdx - 4*int(int32(-offset & mask)).
        let offset = ((instr & EXE_ARM_B_ADDR_MASK) as i32) as i64;
        let mut addr = if instr & EXE_ARM_B_ADDR_SGN_MASK == 0 {
            src_idx as i64 + 4 * offset
        } else {
            src_idx as i64 - 4 * (((-offset) & (EXE_ARM_B_ADDR_MASK as i64)) as i32 as i64)
        };

        if addr < 0 {
            addr = 0;
        }

        let val = opcode1 | ((addr >> 2) as u32);

        if addr == 0 {
            dst[dst_idx..dst_idx + 4].copy_from_slice(&val.to_le_bytes()); // 0 address as escape
            dst[dst_idx + 4..dst_idx + 8].copy_from_slice(&src[src_idx..src_idx + 4]);
            src_idx += 4;
            dst_idx += 8;
            continue;
        }

        dst[dst_idx..dst_idx + 4].copy_from_slice(&val.to_le_bytes());
        src_idx += 4;
        dst_idx += 4;
        matches += 1;
    }

    if matches < 16 {
        return Err("ExeCodec forward transform skip: Too few calls/jumps");
    }

    let count = src.len();

    // Cap expansion due to false positives
    if (src_idx + 4 <= code_end && dst_idx >= dst_end) || dst_idx + (count - src_idx) > dst_end {
        return Err("ExeCodec forward transform skip: Too many false positives");
    }

    dst[1..5].copy_from_slice(&(code_start as u32).to_le_bytes());
    dst[5..9].copy_from_slice(&(dst_idx as u32).to_le_bytes());
    dst[dst_idx..dst_idx + (count - src_idx)].copy_from_slice(&src[src_idx..count]);
    dst_idx += count - src_idx;

    // Cap expansion due to false positives
    if dst_idx > count + (count / 50) {
        return Err("ExeCodec forward transform skip: Too many false positives");
    }

    Ok((count, dst_idx))
}

fn inverse_arm(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
    let mut src_idx = 9usize;
    let mut dst_idx = 0usize;
    let code_start = u32::from_le_bytes(src[1..5].try_into().unwrap()) as usize;
    let code_end = u32::from_le_bytes(src[5..9].try_into().unwrap()) as usize;

    // Sanity check
    if code_end < src_idx
        || code_end > src.len()
        || code_start > code_end - src_idx
        || code_start > dst.len() - dst_idx
    {
        return Err("ExeCodec inverse transform failed: invalid data");
    }

    if code_start > 0 {
        dst[dst_idx..dst_idx + code_start].copy_from_slice(&src[src_idx..src_idx + code_start]);
        dst_idx += code_start;
        src_idx += code_start;
    }

    while src_idx < code_end {
        if src_idx + 4 > code_end {
            return Err("ExeCodec inverse transform failed: invalid data");
        }

        if dst_idx + 4 > dst.len() {
            return Err("ExeCodec inverse transform failed: invalid data");
        }

        let instr = u32::from_le_bytes(src[src_idx..src_idx + 4].try_into().unwrap());
        let opcode1 = instr & EXE_ARM_B_OPCODE_MASK;
        // CBZ/CBNZ arm disabled upstream -- every non-B/BL word is a literal.
        let is_bl = (opcode1 == EXE_ARM_OPCODE_B) || (opcode1 == EXE_ARM_OPCODE_BL);

        if !is_bl {
            // Not a relative jump
            dst[dst_idx..dst_idx + 4].copy_from_slice(&src[src_idx..src_idx + 4]);
            src_idx += 4;
            dst_idx += 4;
            continue;
        }

        // Decode absolute address
        let addr = ((instr & EXE_ARM_B_ADDR_MASK) << 2) as i64;
        let offset = (addr - dst_idx as i64) >> 2;
        let val = opcode1 | ((offset & (EXE_ARM_B_ADDR_MASK as i64)) as u32);

        if addr == 0 {
            if src_idx + 8 > code_end {
                return Err("ExeCodec inverse transform failed: invalid data");
            }

            dst[dst_idx..dst_idx + 4].copy_from_slice(&src[src_idx + 4..src_idx + 8]);
            src_idx += 8;
            dst_idx += 4;
            continue;
        }

        dst[dst_idx..dst_idx + 4].copy_from_slice(&val.to_le_bytes());
        src_idx += 4;
        dst_idx += 4;
    }

    let count = src.len();

    if dst_idx + (count - src_idx) > dst.len() {
        return Err("ExeCodec inverse transform failed: invalid data");
    }

    if src_idx < count {
        dst[dst_idx..dst_idx + (count - src_idx)].copy_from_slice(&src[src_idx..count]);
        dst_idx += count - src_idx;
    }

    Ok((count, dst_idx))
}

/// Detects the executable type. Returns (mode, code_start, code_end) where
/// mode is _EXE_X86/_EXE_ARM64 on success or _EXE_NOT_EXE | dataType bits.
fn detect_exe_type(src: &[u8]) -> (u8, usize, usize) {
    // Let us check the first bytes ... but this may not be the first block
    // Best effort
    let magic = magic::get_magic_type(src);
    let mut arch = 0u32;
    let block_size = src.len();
    let mut code_start = 0usize;
    let mut code_end = block_size;

    if parse_exe_header(src, magic, &mut arch, &mut code_start, &mut code_end) {
        if code_end < code_start || code_end > block_size {
            return (
                EXE_NOT_EXE | DataType::Undefined as u8,
                code_start,
                code_end,
            );
        }

        if arch == EXE_ELF_X86_ARCH as u32 || arch == EXE_ELF_AMD64_ARCH as u32 {
            return (EXE_X86, code_start, code_end);
        }

        if arch == EXE_WIN_X86_ARCH as u32 || arch == EXE_WIN_AMD64_ARCH as u32 {
            return (EXE_X86, code_start, code_end);
        }

        if arch == EXE_MAC_AMD64_ARCH {
            return (EXE_X86, code_start, code_end);
        }

        if arch == EXE_ELF_ARM64_ARCH as u32 || arch == EXE_WIN_ARM64_ARCH as u32 {
            return (EXE_ARM64, code_start, code_end);
        }

        if arch == EXE_MAC_ARM64_ARCH {
            return (EXE_ARM64, code_start, code_end);
        }
    }

    if code_start > block_size || code_end < code_start || code_end > block_size || src.is_empty() {
        return (
            EXE_NOT_EXE | DataType::Undefined as u8,
            code_start,
            code_end,
        );
    }

    let mut jumps_x86 = 0usize;
    let mut jumps_arm64 = 0usize;
    let count = code_end - code_start;
    let mut histo = [0i32; 256];

    let mut i = code_start;

    while i < code_end {
        histo[src[i] as usize] += 1;

        // X86
        if i + 4 < code_end && (src[i] & EXE_X86_MASK_JUMP) == EXE_X86_INSTRUCTION_JUMP {
            if (src[i + 4] == 0) || (src[i + 4] == 0xFF) {
                // Count relative jumps (CALL = E8/ JUMP = E9 .. .. .. 00/FF)
                jumps_x86 += 1;
                i += 1;
                continue;
            }
        } else if src[i] == EXE_X86_TWO_BYTE_PREFIX && i + 1 < code_end {
            let mut j = i + 1;

            if (src[j] == 0x38 || src[j] == 0x3A) && j + 1 < code_end {
                j += 1;
            }

            // Count relative conditional jumps (0x0F 0x8?) with 16/32 offsets
            if (src[j] & EXE_X86_MASK_JCC) == EXE_X86_INSTRUCTION_JCC {
                jumps_x86 += 1;
                i = j + 1;
                continue;
            }

            // No continue: Go falls through to the ARM check at j (byte j
            // stays uncounted in histo -- it is skipped -- but IS examined
            // for ARM opcodes).
            i = j;
        }

        // ARM
        if (i & 3) != 0 || i + 4 > code_end {
            i += 1;
            continue;
        }

        let instr = u32::from_le_bytes(src[i..i + 4].try_into().unwrap());
        let opcode1 = instr & EXE_ARM_B_OPCODE_MASK;
        let opcode2 = instr & EXE_ARM_CB_OPCODE_MASK;

        if (opcode1 == EXE_ARM_OPCODE_B)
            || (opcode1 == EXE_ARM_OPCODE_BL)
            || (opcode2 == EXE_ARM_OPCODE_CBZ)
            || (opcode2 == EXE_ARM_OPCODE_CBNZ)
        {
            jumps_arm64 += 1;
        }

        i += 1;
    }

    let dt = detect_simple_type(count, &histo);

    if dt != DataType::Bin {
        return (EXE_NOT_EXE | dt as u8, code_start, code_end);
    }

    // Filter out (some/many) multimedia files
    let mut small_vals = 0i32;

    for h in &histo[0..16] {
        small_vals += h;
    }

    if histo[0] < (count as i32) / 10
        || small_vals > (count as i32) / 2
        || histo[255] < (count as i32) / 100
    {
        return (EXE_NOT_EXE | dt as u8, code_start, code_end);
    }

    // Ad-hoc thresholds
    if jumps_x86 >= (count / 200) {
        return (EXE_X86, code_start, code_end);
    }

    if jumps_arm64 >= (count / 200) {
        return (EXE_ARM64, code_start, code_end);
    }

    // Number of jump instructions too small => either not an exe or not worth the change, skip.
    (EXE_NOT_EXE | dt as u8, code_start, code_end)
}

fn set_exe_code_range(
    count: usize,
    code_start: &mut usize,
    code_end: &mut usize,
    start: i64,
    length: i64,
) -> bool {
    if start < 0 || length < 0 || start > count as i64 || length > count as i64 - start {
        return false;
    }

    if *code_start == 0 {
        *code_start = start as usize;
    }

    *code_end = (start + length) as usize;
    true
}

/// Returns true on a known header (also narrows the code range). Port of
/// Go's parseExeHeader: WIN/PE, ELF (32/64-bit, LE/BE) and Mach-O (32/64).
fn parse_exe_header(
    src: &[u8],
    magic: u32,
    arch: &mut u32,
    code_start: &mut usize,
    code_end: &mut usize,
) -> bool {
    let count = src.len();

    if magic == magic::WIN_MAGIC {
        if count >= 64 {
            let pos_pe = u32::from_le_bytes(src[60..64].try_into().unwrap()) as usize;

            if pos_pe > 0
                && pos_pe <= count - 48
                && u32::from_le_bytes(src[pos_pe..pos_pe + 4].try_into().unwrap()) == EXE_WIN_PE
            {
                if !set_exe_code_range(
                    count,
                    code_start,
                    code_end,
                    u32::from_le_bytes(src[pos_pe + 44..pos_pe + 48].try_into().unwrap()) as i64,
                    u32::from_le_bytes(src[pos_pe + 28..pos_pe + 32].try_into().unwrap()) as i64,
                ) {
                    return false;
                }

                *arch = u16::from_le_bytes(src[pos_pe + 4..pos_pe + 6].try_into().unwrap()) as u32;
            }

            return true;
        }
    } else if magic == magic::ELF_MAGIC {
        let is_little_endian = src[5] == 1;

        if count >= 64 {
            *code_start = 0;

            if is_little_endian {
                // Little Endian
                if src[4] == 2 {
                    // 64 bits
                    let nb_entries = u16::from_le_bytes(src[0x3C..0x3E].try_into().unwrap()) as i64;
                    let sz_entry = u16::from_le_bytes(src[0x3A..0x3C].try_into().unwrap()) as i64;
                    let pos_section =
                        u64::from_le_bytes(src[0x28..0x30].try_into().unwrap()) as i64;

                    if sz_entry <= 0 || pos_section < 0 || pos_section > count as i64 - 0x28 {
                        return false;
                    }

                    for i in 0..nb_entries {
                        let start_entry = pos_section + i * sz_entry;

                        if start_entry > count as i64 - 0x28 {
                            return false;
                        }

                        let se = start_entry as usize;
                        let type_section =
                            u32::from_le_bytes(src[se + 4..se + 8].try_into().unwrap());
                        let off_section =
                            u64::from_le_bytes(src[se + 0x18..se + 0x20].try_into().unwrap())
                                as i64;
                        let len_section =
                            u64::from_le_bytes(src[se + 0x20..se + 0x28].try_into().unwrap())
                                as i64;

                        if type_section == 1 && len_section >= 64 {
                            if !set_exe_code_range(
                                count,
                                code_start,
                                code_end,
                                off_section,
                                len_section,
                            ) {
                                return false;
                            }
                        }
                    }
                } else {
                    // 32 bits
                    let nb_entries = u16::from_le_bytes(src[0x30..0x32].try_into().unwrap()) as i64;
                    let sz_entry = u16::from_le_bytes(src[0x2E..0x30].try_into().unwrap()) as i64;
                    let pos_section =
                        u32::from_le_bytes(src[0x20..0x24].try_into().unwrap()) as i64;

                    if sz_entry <= 0 || pos_section < 0 || pos_section > count as i64 - 0x18 {
                        return false;
                    }

                    for i in 0..nb_entries {
                        let start_entry = pos_section + i * sz_entry;

                        if start_entry > count as i64 - 0x18 {
                            return false;
                        }

                        let se = start_entry as usize;
                        let type_section =
                            u32::from_le_bytes(src[se + 4..se + 8].try_into().unwrap());
                        let off_section =
                            u32::from_le_bytes(src[se + 0x10..se + 0x14].try_into().unwrap())
                                as i64;
                        let len_section =
                            u32::from_le_bytes(src[se + 0x14..se + 0x18].try_into().unwrap())
                                as i64;

                        if type_section == 1 && len_section >= 64 {
                            if !set_exe_code_range(
                                count,
                                code_start,
                                code_end,
                                off_section,
                                len_section,
                            ) {
                                return false;
                            }
                        }
                    }
                }

                *arch = u16::from_le_bytes(src[18..20].try_into().unwrap()) as u32;
            } else {
                // Big Endian
                if src[4] == 2 {
                    // 64 bits
                    let nb_entries = u16::from_be_bytes(src[0x3C..0x3E].try_into().unwrap()) as i64;
                    let sz_entry = u16::from_be_bytes(src[0x3A..0x3C].try_into().unwrap()) as i64;
                    let pos_section =
                        u64::from_be_bytes(src[0x28..0x30].try_into().unwrap()) as i64;

                    if sz_entry <= 0 || pos_section < 0 || pos_section > count as i64 - 0x28 {
                        return false;
                    }

                    for i in 0..nb_entries {
                        let start_entry = pos_section + i * sz_entry;

                        if start_entry > count as i64 - 0x28 {
                            return false;
                        }

                        let se = start_entry as usize;
                        let type_section =
                            u32::from_be_bytes(src[se + 4..se + 8].try_into().unwrap());
                        let off_section =
                            u64::from_be_bytes(src[se + 0x18..se + 0x20].try_into().unwrap())
                                as i64;
                        let len_section =
                            u64::from_be_bytes(src[se + 0x20..se + 0x28].try_into().unwrap())
                                as i64;

                        if type_section == 1 && len_section >= 64 {
                            if !set_exe_code_range(
                                count,
                                code_start,
                                code_end,
                                off_section,
                                len_section,
                            ) {
                                return false;
                            }
                        }
                    }
                } else {
                    // 32 bits
                    let nb_entries = u16::from_be_bytes(src[0x30..0x32].try_into().unwrap()) as i64;
                    let sz_entry = u16::from_be_bytes(src[0x2E..0x30].try_into().unwrap()) as i64;
                    let pos_section =
                        u32::from_be_bytes(src[0x20..0x24].try_into().unwrap()) as i64;

                    if sz_entry <= 0 || pos_section < 0 || pos_section > count as i64 - 0x18 {
                        return false;
                    }

                    for i in 0..nb_entries {
                        let start_entry = pos_section + i * sz_entry;

                        if start_entry > count as i64 - 0x18 {
                            return false;
                        }

                        let se = start_entry as usize;
                        let type_section =
                            u32::from_be_bytes(src[se + 4..se + 8].try_into().unwrap());
                        let off_section =
                            u32::from_be_bytes(src[se + 0x10..se + 0x14].try_into().unwrap())
                                as i64;
                        let len_section =
                            u32::from_be_bytes(src[se + 0x14..se + 0x18].try_into().unwrap())
                                as i64;

                        if type_section == 1 && len_section >= 64 {
                            if !set_exe_code_range(
                                count,
                                code_start,
                                code_end,
                                off_section,
                                len_section,
                            ) {
                                return false;
                            }
                        }
                    }
                }

                *arch = u16::from_be_bytes(src[18..20].try_into().unwrap()) as u32;
            }

            *code_start = (*code_start).min(count);
            *code_end = (*code_end).min(count);
            return true;
        }
    } else if magic == magic::MAC_MAGIC32
        || magic == magic::MAC_CIGAM32
        || magic == magic::MAC_MAGIC64
        || magic == magic::MAC_CIGAM64
    {
        let is_64_bits = magic == magic::MAC_MAGIC64 || magic == magic::MAC_CIGAM64;
        *code_start = 0;

        if count >= 64 {
            let mode = u32::from_le_bytes(src[12..16].try_into().unwrap());

            if mode != EXE_MAC_MH_EXECUTE {
                return false;
            }

            *arch = u32::from_le_bytes(src[4..8].try_into().unwrap());
            let nb_cmds = u32::from_le_bytes(src[0x10..0x14].try_into().unwrap()) as usize;
            let mut cmd = 0usize;
            let mut pos = 0x1Cusize;

            if is_64_bits {
                pos = 0x20;
            }

            while cmd < nb_cmds {
                if pos > count - 8 {
                    return false;
                }

                let ld_cmd = u32::from_le_bytes(src[pos..pos + 4].try_into().unwrap());
                let sz_cmd = u32::from_le_bytes(src[pos + 4..pos + 8].try_into().unwrap()) as usize;
                let mut sz_seg_hdr = 0x38usize;

                if is_64_bits {
                    sz_seg_hdr = 0x48;
                }

                if sz_cmd < 8 || sz_cmd > count - pos {
                    return false;
                }

                if ld_cmd == EXE_MAC_LC_SEGMENT || ld_cmd == EXE_MAC_LC_SEGMENT64 {
                    if pos > count - 14 || pos > count - sz_seg_hdr {
                        return false;
                    }

                    let name_segment =
                        u64::from_be_bytes(src[pos + 8..pos + 16].try_into().unwrap()) >> 16;

                    if name_segment == 0x5F5F_5445_5854 {
                        let pos_section = pos + sz_seg_hdr;
                        let mut min_section_size = 0x30usize;

                        if is_64_bits {
                            min_section_size = 0x38;
                        }

                        if pos_section > count - min_section_size {
                            return false;
                        }

                        let name_section = u64::from_be_bytes(
                            src[pos_section..pos_section + 8].try_into().unwrap(),
                        ) >> 16;

                        if name_section == 0x5F5F_7465_7874 {
                            // Text section in TEXT segment
                            if is_64_bits {
                                if !set_exe_code_range(
                                    count,
                                    code_start,
                                    code_end,
                                    u64::from_le_bytes(
                                        src[pos_section + 0x30..pos_section + 0x38]
                                            .try_into()
                                            .unwrap(),
                                    ) as i64,
                                    u32::from_le_bytes(
                                        src[pos_section + 0x28..pos_section + 0x2C]
                                            .try_into()
                                            .unwrap(),
                                    ) as i64,
                                ) {
                                    return false;
                                }

                                break;
                            } else {
                                if !set_exe_code_range(
                                    count,
                                    code_start,
                                    code_end,
                                    u32::from_le_bytes(
                                        src[pos_section + 0x2C..pos_section + 0x30]
                                            .try_into()
                                            .unwrap(),
                                    ) as i64,
                                    u32::from_le_bytes(
                                        src[pos_section + 0x28..pos_section + 0x2C]
                                            .try_into()
                                            .unwrap(),
                                    ) as i64,
                                ) {
                                    return false;
                                }

                                break;
                            }
                        }
                    }
                }

                cmd += 1;
                pos += sz_cmd;
            }

            *code_start = (*code_start).min(count);
            *code_end = (*code_end).min(count);
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ARM64 code whose only branches are CBNZ, laid out so the detector's
    /// other gates (data type BIN, enough zero and 0xFF bytes, not too many
    /// small values) pass: what decides the outcome is the CBNZ opcode
    /// alone. A wrong CBNZ constant (this port had 0x03500000 for
    /// 0x35000000) makes the detector miss every branch and decline the
    /// transform -- which cost 0.74% of compressed size at level 4 on
    /// silesia.tar's `mozilla`, where kanzi-cpp applies it.
    fn arm64_block_with_cbnz(n: usize) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut out = Vec::with_capacity(n + 4);

        while out.len() < n {
            let r = next();

            if r % 8 == 0 {
                // One instruction in eight is CBNZ: 0x35 is its opcode byte,
                // which little-endian puts last.
                out.extend_from_slice(&[r as u8, (r >> 8) as u8, (r >> 16) as u8, 0x35]);
                continue;
            }

            // Filler: three data bytes -- ~13% zero and ~3% 0xFF, since the
            // detector wants both, the rest spread over the middle of the
            // range so "small values" stay well under half -- then a fixed
            // high byte that matches no branch opcode, so only the CBNZ
            // instructions above can be counted.
            for k in 0..3 {
                let b = (r >> (8 * k)) as u8;
                out.push(match b % 50 {
                    0..=9 => 0,
                    10 | 11 => 0xFF,
                    _ => 0x10 | (b & 0x7F),
                });
            }

            out.push(0x5A);
        }

        out.truncate(n);
        out
    }

    #[test]
    fn detects_arm64_code_branching_with_cbnz() {
        let data = arm64_block_with_cbnz(1 << 20);
        let (mode, code_start, code_end) = detect_exe_type(&data);
        assert_eq!(mode & EXE_NOT_EXE, 0, "detector declined ARM64 code: mode={mode:#x}");
        assert_eq!(mode, EXE_ARM64, "expected ARM64");
        assert_eq!((code_start, code_end), (0, data.len()));

        // The ARM64 *transform* only rewrites B/BL (CBZ/CBNZ are disabled
        // upstream too), so this block has nothing for it to rewrite and
        // `forward` declines -- detection is what this pins down. Level
        // 4/8/9 round trips cover the transform itself.
        assert!(matches!(
            forward(&data, &mut vec![0u8; max_encoded_len(data.len())], DataType::Undefined),
            Err(("ExeCodec forward transform skip: Too few calls/jumps", _))
        ));
    }
}
