// Port of kanzi-go's SBRT (transform/SBRT.go) -- Sort By Rank Transform,
// used as the RANK stage (mode RANK) at level 5. The struct is generic over
// the three modes (MTF/RANK/TIMESTAMP differ only in mask1/mask2/shift), so
// all three work; L5 uses RANK.

pub const SBRT_MODE_MTF: i32 = 1;
pub const SBRT_MODE_RANK: i32 = 2;
pub const SBRT_MODE_TIMESTAMP: i32 = 3;

pub struct Sbrt {
    mask1: i64,
    mask2: i64,
    shift: u32,
}

impl Sbrt {
    pub fn new_rank() -> Self {
        Sbrt {
            mask1: -1,
            mask2: -1,
            shift: 1,
        }
    }

    pub fn new(mode: i32) -> Result<Self, &'static str> {
        if mode != SBRT_MODE_MTF && mode != SBRT_MODE_RANK && mode != SBRT_MODE_TIMESTAMP {
            return Err("SBRT forward transform failed: invalid mode parameter");
        }

        Ok(Sbrt {
            mask1: if mode == SBRT_MODE_TIMESTAMP { 0 } else { -1 },
            mask2: if mode == SBRT_MODE_MTF { 0 } else { -1 },
            shift: if mode == SBRT_MODE_RANK { 1 } else { 0 },
        })
    }

    pub fn max_encoded_len(src_len: usize) -> usize {
        // Go: srcLen + _BWT_MAX_HEADER_SIZE (33); the header belongs to the
        // BWT block codec, but the bound is reused here for scratch sizing.
        src_len + 33
    }

    pub fn forward(&self, src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
        let count = src.len();

        if count == 0 || dst.is_empty() {
            return Ok((0, 0));
        }

        if count > dst.len() {
            return Err("SBRT forward transform skip: output buffer is too small");
        }

        let mut s2r = [0u8; 256];
        let mut r2s = [0u8; 256];

        for i in 0..256 {
            s2r[i] = i as u8;
            r2s[i] = i as u8;
        }

        let (m1, m2, s) = (self.mask1, self.mask2, self.shift);
        let mut p = [0i64; 256];
        let mut q = [0i64; 256];

        for (i, &b) in src.iter().enumerate() {
            let c = b as usize;
            let r = s2r[c] as usize;
            dst[i] = r as u8;
            // Go: ((i & m1) + (p[c] & m2)) >> s with wrapping int arithmetic.
            let qc = (((i as i64) & m1).wrapping_add(p[c] & m2)) >> s;
            p[c] = i as i64;
            q[c] = qc;

            // Move up symbol to correct rank
            let mut r = r;

            while r > 0 && q[r2s[r - 1] as usize] <= qc {
                let t = r2s[r - 1];
                r2s[r] = t;
                s2r[t as usize] = r as u8;
                r -= 1;
            }

            r2s[r] = b;
            s2r[c] = r as u8;
        }

        Ok((count, count))
    }

    pub fn inverse(&self, src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
        let count = src.len();

        if count == 0 || dst.is_empty() {
            return Ok((0, 0));
        }

        if count > dst.len() {
            return Err("SBRT inverse transform failed: block size is bigger than output buffer");
        }

        let mut r2s = [0u8; 256];

        for i in 0..256 {
            r2s[i] = i as u8;
        }

        let (m1, m2, s) = (self.mask1, self.mask2, self.shift);
        let mut p = [0i64; 256];
        let mut q = [0i64; 256];

        for (i, &b) in src.iter().enumerate() {
            let r = b as usize;
            let c = r2s[r];
            dst[i] = c;
            let qc = (((i as i64) & m1).wrapping_add(p[c as usize] & m2)) >> s;
            p[c as usize] = i as i64;
            q[c as usize] = qc;

            // Move up symbol to correct rank. Go writes only r2s here (the
            // s2r side is not maintained on inverse) -- replicated exactly.
            let mut r = r;

            while r > 0 && q[r2s[r - 1] as usize] <= qc {
                r2s[r] = r2s[r - 1];
                r -= 1;
            }

            r2s[r] = c;
        }

        Ok((count, count))
    }
}
