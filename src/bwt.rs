// Port of kanzi-go's BWT stage for level 5: BWTBlockCodec framing
// (transform/BWTBlockCodec.go) over the BWT core (transform/BWT.go).
//
// Forward suffix-array construction purposefully does NOT port DivSufSort
// (transform/DivSufSort.go, ~2700 lines of induced-sorting): the BWT output
// and primary indexes derive deterministically from the plain suffix array
// (which is unique -- all n suffixes are pairwise distinct strings), so any
// correct SA construction yields byte-identical results. This port builds
// the SA with prefix-doubling + 2-pass radix sort (O(n log n) time, simple
// and obviously correct), verified byte-exact against Go. It is slower than
// DivSufSort on huge blocks; that is the known cost of the smaller port.
//
// Inverse ports inverseMergeTPSI exactly (single- and 8-chunk walks,
// sequential -- jobs=1 like this project's single-job container).
// inverseBiPSIv2 (single blocks > 4MB) is NOT ported and fails loudly;
// only the v6+ block header layout is supported (this project is v7-only,
// and Go takes the same branch for versions 6 and 7).
//
// Wire format (BWTBlockCodec, v6+): [mode:1][primary indexes]
// [bwt data], mode = (logChunks<<2)|(pIndexSize-1); the BWT payload itself
// is [src[n-1]] + all rotation-predecessors except the primary row.

use crate::logtables::TAB_LOG2;

pub const BWT_MAX_HEADER_SIZE: usize = 1 + 8 * 4;
const BWT_BLOCK_SIZE_THRESHOLD1: usize = 256;
const BWT_BLOCK_SIZE_THRESHOLD2: usize = 4 * 1024 * 1024;

pub fn max_encoded_len(src_len: usize) -> usize {
    src_len + BWT_MAX_HEADER_SIZE
}

/// Number of chunks for a block size: 1 below 256 bytes, else 8.
fn get_bwt_chunks(size: usize) -> usize {
    if size < BWT_BLOCK_SIZE_THRESHOLD1 {
        1
    } else {
        8
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

pub struct Bwt {
    buffer: Vec<i32>,
    primary_indexes: [usize; 8],
    // Suffix-array scratch (reused across calls).
    sa: Vec<u32>,
    tmp_sa: Vec<u32>,
    rank: Vec<i32>,
    tmp_rank: Vec<i32>,
    cnt: Vec<u32>,
}

impl Bwt {
    pub fn new() -> Self {
        Bwt {
            buffer: Vec::new(),
            primary_indexes: [0usize; 8],
            sa: Vec::new(),
            tmp_sa: Vec::new(),
            rank: Vec::new(),
            tmp_rank: Vec::new(),
            cnt: Vec::new(),
        }
    }

    /// Builds the suffix array of `src` with prefix-doubling + radix sort.
    /// Standard $-sentinel semantics (a shorter suffix sorts first on shared
    /// prefixes), matching DivSufSort's output exactly.
    fn build_sa(&mut self, src: &[u8]) -> &[u32] {
        let n = src.len();
        debug_assert!(n >= 2);

        if self.sa.len() < n {
            self.sa = vec![0u32; n];
            self.tmp_sa = vec![0u32; n];
            self.rank = vec![0i32; n];
            self.tmp_rank = vec![0i32; n];
        }

        let (sa, rank, tmp_rank) = (
            &mut self.sa[..n],
            &mut self.rank[..n],
            &mut self.tmp_rank[..n],
        );

        for (i, s) in sa.iter_mut().enumerate() {
            *s = i as u32;
        }

        for (i, &b) in src.iter().enumerate() {
            rank[i] = b as i32;
        }

        let mut k = 1usize;

        loop {
            // 2-pass radix sort by (rank[i], rank[i+k] or -1), second key first.
            let max_rank = rank.iter().fold(0i32, |m, &r| m.max(r)) as usize;
            let cnt_len = max_rank + 2; // shifted keys land in [0..max_rank+1]

            if self.cnt.len() < cnt_len {
                self.cnt = vec![0u32; cnt_len];
            }

            let (tmp_sa, cnt) = (&mut self.tmp_sa[..n], &mut self.cnt[..cnt_len]);

            // Pass 1: second key.
            cnt.fill(0);

            for &s in sa.iter() {
                let key = if (s as usize) + k < n {
                    (rank[(s as usize) + k] + 1) as usize
                } else {
                    0
                };
                cnt[key] += 1;
            }

            let mut sum = 0u32;

            for c in cnt.iter_mut() {
                let t = *c;
                *c = sum;
                sum += t;
            }

            for &s in sa.iter() {
                let key = if (s as usize) + k < n {
                    (rank[(s as usize) + k] + 1) as usize
                } else {
                    0
                };
                tmp_sa[cnt[key] as usize] = s;
                cnt[key] += 1;
            }

            // Pass 2: first key (all >= 0, shifted by +1).
            cnt.fill(0);

            for &s in tmp_sa.iter() {
                cnt[(rank[s as usize] + 1) as usize] += 1;
            }

            sum = 0;

            for c in cnt.iter_mut() {
                let t = *c;
                *c = sum;
                sum += t;
            }

            for &s in tmp_sa.iter() {
                let key = (rank[s as usize] + 1) as usize;
                sa[cnt[key] as usize] = s;
                cnt[key] += 1;
            }

            // Re-rank into tmp_rank scratch, then copy back.
            let mut new_max = 0i32;
            tmp_rank[sa[0] as usize] = 0;

            for j in 1..n {
                let a = sa[j - 1] as usize;
                let b = sa[j] as usize;
                let a2 = if a + k < n { rank[a + k] } else { -1 };
                let b2 = if b + k < n { rank[b + k] } else { -1 };

                if rank[a] != rank[b] || a2 != b2 {
                    new_max += 1;
                }

                tmp_rank[b] = new_max;
            }

            rank.copy_from_slice(&tmp_rank[..n]);

            if new_max as usize == n - 1 {
                break;
            }

            k *= 2;
            // Safety net against logic bugs (correct runs terminate with
            // k <= n): loud panic instead of a hang.
            debug_assert!(k < 8 * n);
            assert!(k < 8 * n, "BWT suffix array construction diverged");
        }

        &self.sa[..n]
    }

    /// BWTBlockCodec.Forward: header + BWT data.
    pub fn forward(&mut self, src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
        let count = src.len();

        if count == 0 || dst.is_empty() {
            return Ok((0, 0));
        }

        if dst.len() < max_encoded_len(count) {
            return Err("Output buffer is too small");
        }

        let mut log_block_size = log2_no_check(count as u32);

        if count & (count - 1) != 0 {
            log_block_size += 1;
        }

        let p_index_size = ((log_block_size + 7) >> 3) as usize;

        if p_index_size == 0 || p_index_size >= 5 {
            return Err("BWT forward failed: invalid index size");
        }

        let chunks = get_bwt_chunks(count);
        let log_nb_chunks = log2_no_check(chunks as u32);

        if log_nb_chunks > 7 {
            return Err("BWT forward failed: invalid number of chunks");
        }

        let header_size = chunks * p_index_size + 1;
        // BWT of the whole block (single SA), chunked only for indexing.
        let step = count.div_ceil(chunks);

        // Inverse-indexed SA ranks: rank[pos] for every position.
        let sa = self.build_sa(src).to_vec();
        let mut inv = vec![0u32; count];

        for (r, &s) in sa.iter().enumerate() {
            inv[s as usize] = r as u32;
        }

        let p_idx = inv[0] as usize; // rank of suffix 0 (primary index, 0-based)

        // BWT payload: [src[n-1]] + predecessors except the primary row.
        let out = &mut dst[header_size..header_size + count];
        out[0] = src[count - 1];
        let mut o = 1usize;

        for (i, &s) in sa.iter().enumerate() {
            if i == p_idx {
                continue;
            }

            out[o] = src[(s as usize + count - 1) % count];
            o += 1;
        }

        debug_assert_eq!(o, count);

        // Header: mode + 0-based primary ranks, big-endian.
        let mode = ((log_nb_chunks << 2) | (p_index_size as u32 - 1)) as u8;
        dst[0] = mode;
        let mut idx = 1usize;

        for c in 0..chunks {
            // Rank of the chunk-start suffix (0-based, like Go's stored
            // PrimaryIndex(i)-1).
            let r = inv[(c * step).min(count - 1)] as usize;
            let mut shift = (p_index_size - 1) << 3;

            loop {
                dst[idx] = ((r >> shift) & 0xFF) as u8;
                idx += 1;

                if shift == 0 {
                    break;
                }

                shift -= 8;
            }
        }

        for c in 0..chunks {
            self.primary_indexes[c] = inv[(c * step).min(count - 1)] as usize + 1;
        }

        Ok((count, header_size + count))
    }

    /// BWTBlockCodec.Inverse (v6+ header layout only).
    pub fn inverse(&mut self, src: &[u8], dst: &mut [u8]) -> Result<(usize, usize), &'static str> {
        if src.is_empty() || dst.is_empty() {
            return Ok((0, 0));
        }

        if src.len() == 1 {
            return Err("BWT inverse transform failed: invalid size");
        }

        // Number of chunks and primary index size in bitstream since bsVersion 6
        let mode = src[0];
        let log_nb_chunks = ((mode >> 2) & 0x07) as usize;
        let p_index_size = (mode & 0x03) as usize + 1;
        let chunks = 1usize << log_nb_chunks;
        let header_size = chunks * p_index_size + 1;

        if src.len() < header_size {
            return Err("BWT inverse transform failed: invalid header size");
        }

        let block_size = src.len() - header_size;

        if chunks != get_bwt_chunks(block_size) {
            return Err("BWT inverse transform failed: invalid number of chunks");
        }

        // Read header (stored ranks are 0-based; Go adds +1 via SetPrimaryIndex).
        let mut idx = 1usize;

        for i in 0..chunks {
            let shift0 = (p_index_size - 1) << 3;
            let mut primary_index = 0usize;
            let mut shift = shift0;

            loop {
                primary_index = (primary_index << 8) | src[idx] as usize;
                idx += 1;

                if shift == 0 {
                    break;
                }

                shift -= 8;
            }

            if i >= self.primary_indexes.len() {
                return Err("BWT inverse transform failed: invalid primary index in bitstream");
            }

            self.primary_indexes[i] = primary_index + 1;
        }

        self.inverse_merge_tpsi(&src[header_size..header_size + block_size], dst, block_size)
    }

    /// Port of BWT.inverseMergeTPSI (sequential, jobs=1).
    fn inverse_merge_tpsi(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        count: usize,
    ) -> Result<(usize, usize), &'static str> {
        if count > BWT_BLOCK_SIZE_THRESHOLD2 {
            return Err("BWT inverse transform failed: block too big (BiPSIv2 not ported)");
        }

        if count > dst.len() {
            return Err("BWT inverse transform failed: output buffer too small");
        }

        if count == 1 {
            dst[0] = src[0];
            return Ok((count, count));
        }

        let p_idx = self.primary_indexes[0];

        if p_idx == 0 || p_idx > src.len() {
            return Err("Invalid input: corrupted BWT primary index");
        }

        if self.buffer.len() < count.max(64) {
            self.buffer = vec![0i32; count.max(64)];
        }

        let data = &mut self.buffer[..count];

        // Counting sort into packed (index, value) entries. Blocks < 2^24 by
        // construction (count <= 4MB here), so (i<<8)|val fits i32.
        {
            let mut buckets = [0i32; 256];

            for &b in &src[..count] {
                buckets[b as usize] += 1;
            }

            let mut sum = 0i32;

            for b in buckets.iter_mut() {
                let tmp = *b;
                *b = sum;
                sum += tmp;
            }

            data[buckets[src[0] as usize] as usize] = 0xFF00 | src[0] as i32;
            buckets[src[0] as usize] += 1;

            for i in 1..p_idx {
                let val = src[i] as i32;
                data[buckets[val as usize] as usize] = ((i as i32 - 1) << 8) | val;
                buckets[val as usize] += 1;
            }

            for i in p_idx..count {
                let val = src[i] as i32;
                data[buckets[val as usize] as usize] = (i as i32) << 8 | val;
                buckets[val as usize] += 1;
            }
        }

        if get_bwt_chunks(count) != 8 {
            let mut t = p_idx as i32 - 1;

            for i in 0..count {
                let ptr = data[t as usize];
                dst[i] = ptr as u8;
                t = ptr >> 8;
            }
        } else {
            let mut ck_size = count >> 3;

            if ck_size * 8 != count {
                ck_size += 1;
            }

            let mut t = [0i32; 8];

            for (k, tk) in t.iter_mut().enumerate() {
                let p = self.primary_indexes[k] as i32 - 1;

                if p < 0 || p >= count as i32 {
                    return Err("BWT inverse transform failed: corrupted BWT primary index");
                }

                *tk = p;
            }

            let (d0, rest) = dst.split_at_mut(ck_size);
            let (d1, rest) = rest.split_at_mut(ck_size);
            let (d2, rest) = rest.split_at_mut(ck_size);
            let (d3, rest) = rest.split_at_mut(ck_size);
            let (d4, rest) = rest.split_at_mut(ck_size);
            let (d5, rest) = rest.split_at_mut(ck_size);
            let (d6, rest) = rest.split_at_mut(ck_size);
            let d7 = &mut rest[..count - 7 * ck_size];

            // Last interval [7*chunk:count] smaller when 8*ckSize != count
            let end = count - ck_size * 7;
            let mut n = 0usize;

            while n < end {
                let ptr0 = data[t[0] as usize];
                d0[n] = ptr0 as u8;
                t[0] = ptr0 >> 8;
                let ptr1 = data[t[1] as usize];
                d1[n] = ptr1 as u8;
                t[1] = ptr1 >> 8;
                let ptr2 = data[t[2] as usize];
                d2[n] = ptr2 as u8;
                t[2] = ptr2 >> 8;
                let ptr3 = data[t[3] as usize];
                d3[n] = ptr3 as u8;
                t[3] = ptr3 >> 8;
                let ptr4 = data[t[4] as usize];
                d4[n] = ptr4 as u8;
                t[4] = ptr4 >> 8;
                let ptr5 = data[t[5] as usize];
                d5[n] = ptr5 as u8;
                t[5] = ptr5 >> 8;
                let ptr6 = data[t[6] as usize];
                d6[n] = ptr6 as u8;
                t[6] = ptr6 >> 8;
                let ptr7 = data[t[7] as usize];
                d7[n] = ptr7 as u8;
                t[7] = ptr7 >> 8;
                n += 1;
            }

            while n < ck_size {
                let ptr0 = data[t[0] as usize];
                d0[n] = ptr0 as u8;
                t[0] = ptr0 >> 8;
                let ptr1 = data[t[1] as usize];
                d1[n] = ptr1 as u8;
                t[1] = ptr1 >> 8;
                let ptr2 = data[t[2] as usize];
                d2[n] = ptr2 as u8;
                t[2] = ptr2 >> 8;
                let ptr3 = data[t[3] as usize];
                d3[n] = ptr3 as u8;
                t[3] = ptr3 >> 8;
                let ptr4 = data[t[4] as usize];
                d4[n] = ptr4 as u8;
                t[4] = ptr4 >> 8;
                let ptr5 = data[t[5] as usize];
                d5[n] = ptr5 as u8;
                t[5] = ptr5 >> 8;
                let ptr6 = data[t[6] as usize];
                d6[n] = ptr6 as u8;
                t[6] = ptr6 >> 8;
                n += 1;
            }
        }

        Ok((count, count))
    }
}
