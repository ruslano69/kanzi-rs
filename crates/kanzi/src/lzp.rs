// Port of kanzi-go's LZPCodec (transform/LZCodec.go, Lempel-Ziv-Predict).
// Used twice in level 7's transform chain (LZP+TEXT+UTF+BWT+LZP&CM): once
// as a raw preprocessing pass and once after BWT. Each use gets its own
// `LzpCodec` instance (matching Go's "fresh transform instance per slot"
// pattern already established for the other multi-slot levels).
//
// This project is bitstream-version-7-only, so `isBsVersion3` is always
// false and the inverse always uses MIN_MATCH64 (ported for documentation
// parity with Go; the V3/MIN_MATCH96 branch is dead code here).

const HASH_SEED: u32 = 0x7FEB352D;
const HASH_LOG: u32 = 16;
const HASH_SHIFT: u32 = 32 - HASH_LOG;
const MIN_MATCH64: usize = 64;
const MATCH_FLAG: u8 = 0xFC;
const MIN_BLOCK_LENGTH: usize = 128;

pub struct LzpCodec {
    hashes: Vec<i32>,
}

impl LzpCodec {
    pub fn new() -> Self {
        LzpCodec { hashes: Vec::new() }
    }

    pub fn max_encoded_len(src_len: usize) -> usize {
        if src_len <= 1024 {
            src_len + 16
        } else {
            src_len + src_len / 64
        }
    }

    fn find_match(src: &[u8], src_idx: usize, ref_idx: usize, max_match: usize) -> usize {
        let mut best_len = 0usize;

        while best_len + 8 <= max_match {
            let a = u64::from_le_bytes(src[src_idx + best_len..src_idx + best_len + 8].try_into().unwrap());
            let b = u64::from_le_bytes(src[ref_idx + best_len..ref_idx + best_len + 8].try_into().unwrap());
            let diff = a ^ b;

            if diff != 0 {
                best_len += (diff.trailing_zeros() >> 3) as usize;
                break;
            }

            best_len += 8;
        }

        best_len
    }

    pub fn forward(&mut self, src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
        if src.is_empty() || dst.is_empty() {
            return Ok((0, 0));
        }

        let count = src.len();

        if dst.len() < Self::max_encoded_len(count) {
            return Err("LZP forward transform skip: output buffer too small");
        }

        if count < MIN_BLOCK_LENGTH {
            return Err("Block too small, skip");
        }

        let src_end = count;
        let dst_end = count - (count >> 6);

        if self.hashes.is_empty() {
            self.hashes = vec![0i32; 1 << HASH_LOG];
        } else {
            self.hashes.iter_mut().for_each(|h| *h = 0);
        }

        dst[0] = src[0];
        dst[1] = src[1];
        dst[2] = src[2];
        dst[3] = src[3];
        let mut ctx = u32::from_le_bytes(src[0..4].try_into().unwrap());
        let mut src_idx = 4usize;
        let mut dst_idx = 4usize;

        while src_idx < src_end.saturating_sub(MIN_MATCH64) && dst_idx < dst_end {
            let h = (HASH_SEED.wrapping_mul(ctx) >> HASH_SHIFT) as usize;
            let ref_idx = self.hashes[h] as usize;
            self.hashes[h] = src_idx as i32;
            let mut best_len = 0usize;

            if ref_idx != 0
                && u64::from_le_bytes(src[src_idx + MIN_MATCH64 - 8..src_idx + MIN_MATCH64].try_into().unwrap())
                    == u64::from_le_bytes(src[ref_idx + MIN_MATCH64 - 8..ref_idx + MIN_MATCH64].try_into().unwrap())
            {
                best_len = Self::find_match(src, src_idx, ref_idx, src_end - src_idx);
            }

            if best_len < MIN_MATCH64 {
                let val = src[src_idx];
                ctx = (ctx << 8) | val as u32;
                dst[dst_idx] = src[src_idx];
                src_idx += 1;
                dst_idx += 1;

                if ref_idx != 0 && val == MATCH_FLAG {
                    if dst_idx >= dst_end {
                        return Err("LZP forward transform skip: output buffer too small");
                    }

                    dst[dst_idx] = 0xFF;
                    dst_idx += 1;
                }

                continue;
            }

            src_idx += best_len;
            ctx = u32::from_le_bytes(src[src_idx - 4..src_idx].try_into().unwrap());
            dst[dst_idx] = MATCH_FLAG;
            dst_idx += 1;
            let mut best_len = best_len - MIN_MATCH64;

            while best_len >= 254 {
                best_len -= 254;
                dst[dst_idx] = 0xFE;
                dst_idx += 1;

                if dst_idx >= dst_end {
                    break;
                }
            }

            if dst_idx >= dst_end {
                return Err("LZP forward transform skip: output buffer too small");
            }

            dst[dst_idx] = best_len as u8;
            dst_idx += 1;
        }

        while src_idx < src_end && dst_idx < dst_end {
            let h = (HASH_SEED.wrapping_mul(ctx) >> HASH_SHIFT) as usize;
            let ref_idx = self.hashes[h];
            self.hashes[h] = src_idx as i32;
            let val = src[src_idx];
            ctx = (ctx << 8) | val as u32;
            dst[dst_idx] = src[src_idx];
            src_idx += 1;
            dst_idx += 1;

            if ref_idx != 0 && val == MATCH_FLAG {
                if dst_idx >= dst_end {
                    return Err("LZP forward transform skip: output buffer too small");
                }

                dst[dst_idx] = 0xFF;
                dst_idx += 1;
            }
        }

        if src_idx != count || dst_idx >= dst_end {
            return Err("LZP forward transform skip: output buffer too small");
        }

        Ok((src_idx, dst_idx))
    }

    pub fn inverse(&mut self, src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
        if src.is_empty() || dst.is_empty() {
            return Ok((0, 0));
        }

        if src.len() < 4 {
            return Err("LZP inverse transform failed: block too small");
        }

        if dst.len() < src.len() {
            return Err("LZP inverse transform failed: output buffer too small");
        }

        if self.hashes.is_empty() {
            self.hashes = vec![0i32; 1 << HASH_LOG];
        } else {
            self.hashes.iter_mut().for_each(|h| *h = 0);
        }

        let src_end = src.len();
        let dst_end = dst.len();
        dst[0] = src[0];
        dst[1] = src[1];
        dst[2] = src[2];
        dst[3] = src[3];
        let mut ctx = u32::from_le_bytes(dst[0..4].try_into().unwrap());
        let mut src_idx = 4usize;
        let mut dst_idx = 4usize;
        let mut ok = true;
        let min_match = MIN_MATCH64;

        while src_idx < src_end {
            let h = (HASH_SEED.wrapping_mul(ctx) >> HASH_SHIFT) as usize;
            let ref_idx = self.hashes[h] as usize;
            self.hashes[h] = dst_idx as i32;

            if src[src_idx] != MATCH_FLAG || ref_idx == 0 {
                if dst_idx >= dst_end {
                    return Err("LZP inverse transform failed: output buffer too small");
                }

                dst[dst_idx] = src[src_idx];
                ctx = (ctx << 8) | dst[dst_idx] as u32;
                src_idx += 1;
                dst_idx += 1;
                continue;
            }

            src_idx += 1;

            if src_idx >= src_end {
                return Err("LZP inverse transform failed: invalid data");
            }

            if src[src_idx] == 0xFF {
                if dst_idx >= dst_end {
                    return Err("LZP inverse transform failed: output buffer too small");
                }

                dst[dst_idx] = MATCH_FLAG;
                ctx = (ctx << 8) | MATCH_FLAG as u32;
                src_idx += 1;
                dst_idx += 1;
                continue;
            }

            let mut m_len = min_match;

            if src[src_idx] == 0xFE {
                while src_idx < src_end && src[src_idx] == 0xFE {
                    src_idx += 1;
                    m_len += 254;
                }

                if src_idx >= src_end {
                    ok = false;
                    break;
                }
            }

            m_len += src[src_idx] as usize;
            src_idx += 1;
            let m_end = dst_idx + m_len;

            if m_end > dst_end {
                ok = false;
                break;
            }

            // Overlapping copy (ref_idx + m_len may exceed dst_idx), so copy
            // byte-by-byte like Go's fallback branch always would be safe;
            // Go special-cases the non-overlapping case for speed via
            // `copy`, which we mirror with copy_within when safe.
            if ref_idx + m_len < dst_idx {
                dst.copy_within(ref_idx..ref_idx + m_len, dst_idx);
            } else {
                for i in 0..m_len {
                    dst[dst_idx + i] = dst[ref_idx + i];
                }
            }

            dst_idx += m_len;
            ctx = u32::from_le_bytes(dst[dst_idx - 4..dst_idx].try_into().unwrap());
        }

        if !ok || src_idx != src_end {
            return Err("LZP inverse transform failed: output buffer too small");
        }

        Ok((src_idx, dst_idx))
    }
}
