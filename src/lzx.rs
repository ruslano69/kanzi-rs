// Port of kanzi-go's LZXCodec (transform/LZCodec.go). Handles both the
// extra=true variant (LZX_TYPE, levels 1/3: bigger hash table, also probes
// position+2 during lazy matching) and extra=false (LZ_TYPE, level 2: used
// after the DNA/Alias stage). LZP is not implemented -- not needed for l1-l3.

const HASH_SEED: u64 = 0x1E35A7BD;
const HASH_LOG1: u32 = 16;
const HASH_RSHIFT1: u32 = 64 - HASH_LOG1;
const HASH_LSHIFT1: u32 = 24;
const HASH_LOG2: u32 = 19;
const HASH_RSHIFT2: u32 = 64 - HASH_LOG2;
const HASH_LSHIFT2: u32 = 24;
const MAX_DISTANCE1: i64 = (1 << 16) - 2;
const MAX_DISTANCE2: i64 = (1 << 24) - 2;
pub const MIN_MATCH4: i64 = 4;
pub const MIN_MATCH6: i64 = 6;
const MAX_MATCH: i64 = 65535 + 254 + 4;
const MIN_BLOCK_LENGTH: usize = 24;
pub const READ_LENGTH_GUARD: usize = 4;

pub struct LzxCodec {
    extra: bool,
    hashes: Vec<i32>,
    m_len_buf: Vec<u8>,
    m_buf: Vec<u8>,
    tk_buf: Vec<u8>,
}

impl LzxCodec {
    /// `extra` selects LZX_TYPE (true, bigger hash table + position+2 lazy
    /// probe) vs LZ_TYPE (false) -- must match what the real encoder used.
    pub fn new(extra: bool) -> Self {
        LzxCodec {
            extra,
            hashes: Vec::new(),
            m_len_buf: Vec::new(),
            m_buf: Vec::new(),
            tk_buf: Vec::new(),
        }
    }

    pub fn max_encoded_len(src_len: usize) -> usize {
        if src_len <= 1024 {
            src_len + 16 + READ_LENGTH_GUARD
        } else {
            src_len + src_len / 64 + READ_LENGTH_GUARD
        }
    }

    #[inline]
    fn hash(&self, p: &[u8]) -> usize {
        // Go computes this as a wrapping 64-bit multiply (`*` on uint64
        // silently wraps mod 2^64) then keeps the top bits via >>rshift --
        // NOT a widening 128-bit product.
        let v = u64::from_le_bytes(p[0..8].try_into().unwrap());

        if self.extra {
            (v.wrapping_shl(HASH_LSHIFT2).wrapping_mul(HASH_SEED) >> HASH_RSHIFT2) as usize
        } else {
            (v.wrapping_shl(HASH_LSHIFT1).wrapping_mul(HASH_SEED) >> HASH_RSHIFT1) as usize
        }
    }

    /// Forward transform. `min_match` is MIN_MATCH4 normally, or MIN_MATCH6
    /// when the caller has classified the block as DNA data (see the
    /// dataType handling in dna.rs) -- matches LZCodec.Forward's ctx lookup.
    /// Returns (bytes_read, bytes_written) or an error message (mirrors
    /// Go's "skip" errors -- caller should treat any Err as "this transform
    /// declined, copy the block unchanged").
    pub fn forward(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        min_match: i64,
    ) -> Result<(usize, usize), &'static str> {
        if src.is_empty() || dst.is_empty() {
            return Ok((0, 0));
        }

        let count = src.len();

        if dst.len() < Self::max_encoded_len(count) {
            return Err("output buffer too small");
        }

        if count < MIN_BLOCK_LENGTH {
            return Err("block too small, skip");
        }

        if self.hashes.is_empty() {
            self.hashes = vec![0i32; 1usize << (if self.extra { HASH_LOG2 } else { HASH_LOG1 })];
        } else {
            self.hashes.iter_mut().for_each(|h| *h = 0);
        }

        let min_buf_size = (count / 5).max(256);

        if self.m_len_buf.len() < min_buf_size {
            self.m_len_buf = vec![0u8; min_buf_size];
        }

        if self.m_buf.len() < min_buf_size {
            self.m_buf = vec![0u8; min_buf_size];
        }

        if self.tk_buf.len() < min_buf_size {
            self.tk_buf = vec![0u8; min_buf_size];
        }

        let src_end = count as i64 - 16 - 2;
        let mut max_dist = MAX_DISTANCE2;
        dst[12] = 1;

        if src_end < 4 * MAX_DISTANCE1 {
            max_dist = MAX_DISTANCE1;
            dst[12] = 0;
        }

        dst[12] |= (((min_match - 2) & 0x07) << 1) as u8;

        let mut src_idx: i64 = 0;
        let mut dst_idx: usize = 13;
        let mut anchor: i64 = 0;
        let mut m_len_idx = 0usize;
        let mut m_idx = 0usize;
        let mut tk_idx = 0usize;
        let mut repd = [count as i64, count as i64];
        let mut repd_idx = 0usize;
        let mut src_inc: i64 = 0;

        while src_idx < src_end {
            let mut best_len: i64 = 0;
            let h0 = self.hash(&src[src_idx as usize..]);
            let ref0 = self.hashes[h0] as i64;
            self.hashes[h0] = src_idx as i32;
            let p = u64::from_le_bytes(
                src[src_idx as usize..src_idx as usize + 8]
                    .try_into()
                    .unwrap(),
            );
            let src_idx1 = src_idx + 1;
            let max_match = (src_end - src_idx1).min(MAX_MATCH);
            let mut r = src_idx1 - repd[repd_idx];
            let min_ref = (src_idx - max_dist).max(0);

            if r > min_ref
                && (p >> 8) as u32
                    == u32::from_le_bytes(src[r as usize..r as usize + 4].try_into().unwrap())
            {
                best_len = find_match(src, src_idx1, r, max_match);
            } else {
                r = src_idx1 - repd[repd_idx ^ 1];

                if r > min_ref
                    && (p >> 8) as u32
                        == u32::from_le_bytes(src[r as usize..r as usize + 4].try_into().unwrap())
                {
                    best_len = find_match(src, src_idx1, r, max_match);
                }
            }

            let mut refv = r;

            if best_len < min_match {
                refv = ref0;
                let mut matched = false;

                if refv > min_ref
                    && p as u32
                        == u32::from_le_bytes(
                            src[refv as usize..refv as usize + 4].try_into().unwrap(),
                        )
                {
                    best_len = find_match(src, src_idx, refv, (src_end - src_idx).min(MAX_MATCH));

                    if best_len >= min_match {
                        matched = true;
                    }
                }

                if !matched {
                    src_idx = src_idx1 + (src_inc >> 6);
                    src_inc += 1;
                    repd_idx = 0;
                    continue;
                }

                // checkNext
                if refv != src_idx - repd[0] && refv != src_idx - repd[1] {
                    let h1 = self.hash(&src[src_idx1 as usize..]);
                    let ref1 = self.hashes[h1] as i64;
                    self.hashes[h1] = src_idx1 as i32;

                    if ref1 > min_ref + 1
                        && u32::from_le_bytes(
                            src[(src_idx1 + best_len - 3) as usize
                                ..(src_idx1 + best_len - 3) as usize + 4]
                                .try_into()
                                .unwrap(),
                        ) == u32::from_le_bytes(
                            src[(ref1 + best_len - 3) as usize..(ref1 + best_len - 3) as usize + 4]
                                .try_into()
                                .unwrap(),
                        )
                    {
                        let best_len1 = find_match(src, src_idx1, ref1, max_match);

                        if best_len1 >= best_len {
                            refv = ref1;
                            best_len = best_len1;
                            src_idx = src_idx1;
                        }
                    }

                    if self.extra {
                        // Check if better match at position+2
                        let src_idx2 = src_idx1 + 1;
                        let h2 = self.hash(&src[src_idx2 as usize..]);
                        let ref2 = self.hashes[h2] as i64;
                        self.hashes[h2] = src_idx2 as i32;

                        if ref2 > min_ref + 2
                            && u32::from_le_bytes(
                                src[(src_idx2 + best_len - 3) as usize
                                    ..(src_idx2 + best_len - 3) as usize + 4]
                                    .try_into()
                                    .unwrap(),
                            ) == u32::from_le_bytes(
                                src[(ref2 + best_len - 3) as usize
                                    ..(ref2 + best_len - 3) as usize + 4]
                                    .try_into()
                                    .unwrap(),
                            )
                        {
                            let best_len2 = find_match(
                                src,
                                src_idx2,
                                ref2,
                                (src_end - src_idx2).min(MAX_MATCH),
                            );

                            if best_len2 >= best_len {
                                refv = ref2;
                                best_len = best_len2;
                                src_idx = src_idx2;
                            }
                        }
                    }
                }

                // Extend backwards
                while src_idx > anchor
                    && refv > min_ref
                    && src[(src_idx - 1) as usize] == src[(refv - 1) as usize]
                {
                    best_len += 1;
                    refv -= 1;
                    src_idx -= 1;
                }

                if best_len > MAX_MATCH {
                    src_idx += best_len - MAX_MATCH;
                    refv += best_len - MAX_MATCH;
                    best_len = MAX_MATCH;
                }
            } else {
                if src[src_idx as usize] == src[(refv - 1) as usize] && best_len < MAX_MATCH {
                    best_len += 1;
                    refv -= 1;
                } else {
                    src_idx += 1;
                    let h1 = self.hash(&src[src_idx as usize..]);
                    self.hashes[h1] = src_idx as i32;
                }
            }

            // Emit match
            src_inc = 0;
            let dist = src_idx - refv;
            let m_len = best_len - min_match;
            let token;
            let m_len_th;

            if dist == repd[0] {
                token = 0x00i64;
                m_len_th = 3i64;
            } else if dist == repd[1] {
                token = 0x04i64;
                m_len_th = 3i64;
            } else {
                m_len_th = 7i64;
                let encoded_dist = dist - 1;

                if encoded_dist < 256 {
                    self.m_buf[m_idx] = encoded_dist as u8;
                    m_idx += 1;
                    token = 0x08;
                } else if encoded_dist < 65792 {
                    let value = ((encoded_dist - 256) as u32) << 16;
                    self.m_buf[m_idx..m_idx + 4].copy_from_slice(&value.to_be_bytes());
                    m_idx += 2;
                    token = 0x10;
                } else {
                    let value = ((encoded_dist - 65792) as u32) << 8;
                    self.m_buf[m_idx..m_idx + 4].copy_from_slice(&value.to_be_bytes());
                    m_idx += 3;
                    token = 0x18;
                }
            }

            let mut token = token;

            if m_len >= m_len_th {
                token += m_len_th;
                m_len_idx += emit_length(
                    &mut self.m_len_buf[m_len_idx..],
                    (m_len - m_len_th) as usize,
                );
            } else {
                token += m_len;
            }

            repd[1] = repd[0];
            repd[0] = dist;
            repd_idx = 1;
            let lit_len = src_idx - anchor;

            if lit_len == 0 {
                self.tk_buf[tk_idx] = token as u8;
                tk_idx += 1;
            } else {
                if lit_len >= 7 {
                    if lit_len >= 1 << 24 {
                        return Err("too many literals");
                    }

                    self.tk_buf[tk_idx] = ((7 << 5) | token) as u8;
                    tk_idx += 1;
                    dst_idx += emit_length(&mut dst[dst_idx..], (lit_len - 7) as usize);
                } else {
                    self.tk_buf[tk_idx] = ((lit_len << 5) | token) as u8;
                    tk_idx += 1;
                }

                dst[dst_idx..dst_idx + lit_len as usize]
                    .copy_from_slice(&src[anchor as usize..(anchor + lit_len) as usize]);
                dst_idx += lit_len as usize;
            }

            if m_idx >= self.m_buf.len() - 8 {
                let extra1 = vec![0u8; self.m_buf.len() / 2];
                self.m_buf.extend_from_slice(&extra1);

                if m_len_idx >= self.m_len_buf.len() - 8 {
                    let extra2 = vec![0u8; self.m_len_buf.len() / 2];
                    self.m_len_buf.extend_from_slice(&extra2);
                }
            }

            anchor = src_idx + best_len;
            let mut hash_idx = src_idx + 1;

            while hash_idx + 6 < anchor {
                let hi = hash_idx as usize;
                let a = self.hash(&src[hi..]);
                let b = self.hash(&src[hi + 3..]);
                let c = self.hash(&src[hi + 5..]);
                let d = self.hash(&src[hi + 6..]);
                self.hashes[a] = hash_idx as i32;
                self.hashes[b] = (hash_idx + 3) as i32;
                self.hashes[c] = (hash_idx + 5) as i32;
                self.hashes[d] = (hash_idx + 6) as i32;
                hash_idx += 8;
            }

            while hash_idx < anchor {
                let h = self.hash(&src[hash_idx as usize..]);
                self.hashes[h] = hash_idx as i32;
                hash_idx += 1;
            }

            src_idx = anchor;
        }

        // Emit last literals
        let lit_len = count as i64 - anchor;

        if dst_idx + lit_len as usize + tk_idx + m_idx + m_len_idx >= count {
            return Err("no compression");
        }

        if lit_len >= 7 {
            self.tk_buf[tk_idx] = (7u8) << 5;
            tk_idx += 1;
            dst_idx += emit_length(&mut dst[dst_idx..], (lit_len - 7) as usize);
        } else {
            self.tk_buf[tk_idx] = (lit_len as u8) << 5;
            tk_idx += 1;
        }

        dst[dst_idx..dst_idx + lit_len as usize]
            .copy_from_slice(&src[anchor as usize..(anchor + lit_len) as usize]);
        dst_idx += lit_len as usize;

        dst[0..4].copy_from_slice(&(dst_idx as u32).to_le_bytes());
        dst[4..8].copy_from_slice(&(tk_idx as u32).to_le_bytes());
        dst[8..12].copy_from_slice(&(m_idx as u32).to_le_bytes());
        dst[dst_idx..dst_idx + tk_idx].copy_from_slice(&self.tk_buf[0..tk_idx]);
        dst_idx += tk_idx;
        dst[dst_idx..dst_idx + m_idx].copy_from_slice(&self.m_buf[0..m_idx]);
        dst_idx += m_idx;
        dst[dst_idx..dst_idx + m_len_idx].copy_from_slice(&self.m_len_buf[0..m_len_idx]);
        dst_idx += m_len_idx;

        if dst_idx > count - count / 100 {
            return Err("no compression");
        }

        Ok((count, dst_idx))
    }

    /// Inverse transform (V7 wire format). `src` must have at least
    /// READ_LENGTH_GUARD bytes of valid capacity past `src.len()` (zeroed is
    /// fine -- see lzx.rs module doc in the Go source: the extra bytes are
    /// always masked/shifted away in every code path that reads them).
    pub fn inverse(src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
        if src.is_empty() || dst.is_empty() {
            return Ok((0, 0));
        }

        let count = src.len();

        if count < 13 {
            return Err("invalid data");
        }

        let tk_idx0 = u32::from_le_bytes(src[0..4].try_into().unwrap()) as i64;
        let m_idx0 = u32::from_le_bytes(src[4..8].try_into().unwrap()) as i64;
        let m_len_idx0 = u32::from_le_bytes(src[8..12].try_into().unwrap()) as i64;

        if tk_idx0 <= 13
            || tk_idx0 as usize > count
            || m_idx0 as usize > count - tk_idx0 as usize
            || m_len_idx0 as usize > count - tk_idx0 as usize - m_idx0 as usize
        {
            return Err("invalid data");
        }

        let mut tk_idx = tk_idx0 as usize;
        let mut m_idx = (m_idx0 + tk_idx0) as usize;
        let mut m_len_idx = (m_len_idx0 + m_idx0 + tk_idx0) as usize;

        let token_end = m_idx;
        let match_end = m_len_idx;
        let src_end = tk_idx as i64 - 13;
        let lit_end = tk_idx;
        let dst_limit = dst.len() as i64;
        let m_flag = src[12] & 0x01;
        let dst_end = dst.len() as i64 - 16;
        let max_dist = if m_flag == 0 {
            MAX_DISTANCE1
        } else {
            MAX_DISTANCE2
        };
        let min_match = (((src[12] >> 1) & 0x07) as i64) + 2;

        let mut src_idx: i64 = 13;
        let mut dst_idx: i64 = 0;
        let mut repd0: i64 = count as i64;
        let mut repd1: i64 = count as i64;
        let dist_shift: [u32; 4] = [0, 24, 16, 8];
        let dist_bias: [i64; 4] = [0, 1, 257, 65793];

        loop {
            let token = src[tk_idx] as i64;
            tk_idx += 1;

            let mut m_len;
            let dist;
            let f = token & 0x18;

            if f == 0 {
                m_len = token & 0x03;

                if m_len == 3 {
                    if m_len_idx >= count {
                        return Err("invalid match length");
                    }

                    let (ml, ml_len) = read_length(&src[m_len_idx..]);
                    m_len += min_match + ml as i64;
                    m_len_idx += ml_len;
                } else {
                    m_len += min_match;
                }

                dist = if token & 0x04 == 0 { repd0 } else { repd1 };
            } else {
                m_len = token & 0x07;

                if m_len == 7 {
                    if m_len_idx >= count {
                        return Err("invalid match length");
                    }

                    let (ml, ml_len) = read_length(&src[m_len_idx..]);
                    m_len += min_match + ml as i64;
                    m_len_idx += ml_len;
                } else {
                    m_len += min_match;
                }

                if m_idx >= count {
                    return Err("invalid distance");
                }

                let width = ((token >> 3) & 3) as usize;
                let avail = count - m_idx;
                // Only the top `width` bytes of `value` are ever used (see
                // dist_shift below) -- Go's original always reads a full
                // uint32 here (LZCodec.go), relying on scratch-buffer slop
                // that survives past the officially-counted match section;
                // this port's exact-length buffers don't provide that, so
                // for the tail entry (avail < 4, always avail >= width)
                // read only what's really there and zero-fill the rest,
                // which cannot change `dist` since those bytes are shifted
                // away regardless of their value.
                let value = if avail >= 4 {
                    u32::from_be_bytes(src[m_idx..m_idx + 4].try_into().unwrap())
                } else {
                    let mut buf = [0u8; 4];
                    buf[..avail].copy_from_slice(&src[m_idx..count]);
                    u32::from_be_bytes(buf)
                };
                dist = ((value >> dist_shift[width]) as i64) + dist_bias[width];
                m_idx += width;
            }

            if token >= 32 {
                let lit_len: i64;

                if token >= 0xE0 {
                    let (ll, ll_len) = read_length(&src[src_idx as usize..]);
                    lit_len = 7 + ll as i64;
                    src_idx += ll_len as i64;
                } else {
                    lit_len = token >> 5;
                }

                if lit_len > dst_limit - dst_idx || lit_len > lit_end as i64 - src_idx {
                    return Err("invalid literal length");
                }

                dst[dst_idx as usize..(dst_idx + lit_len) as usize]
                    .copy_from_slice(&src[src_idx as usize..(src_idx + lit_len) as usize]);

                src_idx += lit_len;
                dst_idx += lit_len;

                if src_idx >= src_end {
                    break;
                }
            }

            repd1 = repd0;
            repd0 = dist;
            let m_end = dst_idx + m_len;
            let mut refv = dst_idx - dist;

            if refv < 0 || dist > max_dist || m_end > dst_end {
                return Err("invalid distance decoded");
            }

            if dist >= 16 {
                loop {
                    dst.copy_within(refv as usize..refv as usize + 16, dst_idx as usize);
                    refv += 16;
                    dst_idx += 16;

                    if dst_idx >= m_end {
                        break;
                    }
                }
            } else {
                for i in 0..m_len {
                    dst[(dst_idx + i) as usize] = dst[(refv + i) as usize];
                }
            }

            dst_idx = m_end;
        }

        if src_idx != src_end + 13
            || tk_idx != token_end
            || m_idx != match_end
            || m_len_idx != count
        {
            return Err("inverse transform failed");
        }

        Ok((m_idx, dst_idx as usize))
    }
}

fn find_match(src: &[u8], src_idx: i64, refv: i64, max_match: i64) -> i64 {
    let mut best_len = 0i64;

    while best_len + 8 <= max_match {
        let a = u64::from_le_bytes(
            src[(src_idx + best_len) as usize..(src_idx + best_len) as usize + 8]
                .try_into()
                .unwrap(),
        );
        let b = u64::from_le_bytes(
            src[(refv + best_len) as usize..(refv + best_len) as usize + 8]
                .try_into()
                .unwrap(),
        );
        let diff = a ^ b;

        if diff != 0 {
            best_len += (diff.trailing_zeros() >> 3) as i64;
            break;
        }

        best_len += 8;
    }

    best_len
}

fn emit_length(block: &mut [u8], length: usize) -> usize {
    if length < 254 {
        block[0] = length as u8;
        return 1;
    }

    if length < 65536 + 254 {
        let l = length - 254;
        block[0] = 254;
        block[1] = (l >> 8) as u8;
        block[2] = l as u8;
        return 3;
    }

    let l = length - 255;
    block[0] = 255;
    block[1] = (l >> 16) as u8;
    block[2] = (l >> 8) as u8;
    block[3] = l as u8;
    4
}

fn read_length(block: &[u8]) -> (usize, usize) {
    let mut res = block[0] as usize;

    if res < 254 {
        return (res, 1);
    }

    if res == 254 {
        res += (block[1] as usize) << 8;
        res += block[2] as usize;
        return (res, 3);
    }

    res += (block[1] as usize) << 16;
    res += (block[2] as usize) << 8;
    res += block[3] as usize;
    (res, 4)
}
