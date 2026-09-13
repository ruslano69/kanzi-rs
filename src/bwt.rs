// Port of kanzi-go's BWT stage for level 5: BWTBlockCodec framing
// (transform/BWTBlockCodec.go) over the BWT core (transform/BWT.go).
//
// Forward suffix-array construction purposefully does NOT port DivSufSort
// (transform/DivSufSort.go, ~2700 lines of Go/Java-specific induced-sorting
// bookkeeping): the BWT output and primary indexes derive deterministically
// from the plain suffix array (which is unique -- all n suffixes are
// pairwise distinct strings), so any correct SA construction yields
// byte-identical results. This port builds the SA with SA-IS instead (see
// sais.rs) -- a different, from-scratch, well-documented induced-sorting
// algorithm in the same near-linear complexity class as DivSufSort, chosen
// over a literal port for much lower risk of mistranslating DivSufSort's
// specific micro-optimizations. (An earlier version of this port used
// prefix-doubling + 2-pass radix sort, O(n log n) and noticeably slower on
// large blocks; SA-IS replaced it for that reason.)
//
// Inverse ports inverseMergeTPSI exactly (single- and 8-chunk walks,
// sequential -- jobs=1 like this project's single-job container). Go
// switches to inverseBiPSIv2 above 4MB purely for performance -- both
// algorithms compute the same mathematically unique inverse permutation
// (given a correct primary index), so any block MergeTPSI can pack
// correctly decodes identically to what BiPSIv2 would have produced.
// MergeTPSI's own limit is its (index<<8)|value packing into one i32,
// which needs `index` (up to count-1) to fit in 24 bits, i.e. count <=
// 1<<24 (16 MiB) -- covers every level 5/6/7 default block size (4/8/16
// MiB). Go's comment on this packing says the same 2^24 bound but Go
// reads it back with an arithmetic (sign-propagating) shift, which
// actually only stays correct up to count <= 1<<23 since Go never
// exercises this path above its own 4MB algorithm-choice threshold; this
// port reads it back with a logical (unsigned) shift instead, so it is
// not bound by that narrower accidental limit and safely spans the full
// 1<<24 the packing scheme was designed for. inverseBiPSIv2 itself (a
// different, more memory-efficient algorithm needed only above 1<<24) is
// NOT ported and fails loudly. Only the v6+ block header layout is
// supported (this project is v7-only, and Go takes the same branch for
// versions 6 and 7).
//
// Wire format (BWTBlockCodec, v6+): [mode:1][primary indexes]
// [bwt data], mode = (logChunks<<2)|(pIndexSize-1); the BWT payload itself
// is [src[n-1]] + all rotation-predecessors except the primary row.

use crate::logtables::TAB_LOG2;

/// Suffix-array construction backend for the forward BWT.
///
/// Default: the in-tree SA-IS (`sais.rs`). With the `fast-sa` feature, the
/// libsais C library is used instead -- it documents the exact same
/// sentinel convention the BWT stage needs ("sorts suffixes as if a unique,
/// lexicographically smallest character were present at the end of the
/// text"), so it is a drop-in replacement that yields the identical SA.
#[cfg(feature = "fast-sa")]
fn build_suffix_array(src: &[u8]) -> Vec<u32> {
    use libsais::SuffixArrayConstruction;

    // Single-threaded: the container already encodes blocks concurrently,
    // so adding libsais' own parallelism per block only oversubscribes the
    // machine. It also keeps the build free of the OpenMP runtime (the
    // dependency is declared with `default-features = false`).
    let sa: Vec<i32> = SuffixArrayConstruction::for_text(src)
        .in_owned_buffer()
        .single_threaded()
        .run()
        .expect("libsais suffix array construction failed")
        .into_vec();

    sa.into_iter().map(|x| x as u32).collect()
}

#[cfg(not(feature = "fast-sa"))]
fn build_suffix_array(src: &[u8]) -> Vec<u32> {
    crate::sais::suffix_array(src)
}

pub const BWT_MAX_HEADER_SIZE: usize = 1 + 8 * 4;
const BWT_BLOCK_SIZE_THRESHOLD1: usize = 256;
// True correctness bound of inverseMergeTPSI's (index<<8)|value packing
// (see the module doc comment) -- NOT Go's 4MB algorithm-choice threshold.
const BWT_MERGE_TPSI_MAX: usize = 1 << 24;

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
    // Suffix-array result (owned across calls so its allocation is reused
    // by the next block handled by the same worker).
    sa: Vec<u32>,
}

impl Bwt {
    pub fn new() -> Self {
        Bwt { buffer: Vec::new(), primary_indexes: [0usize; 8], sa: Vec::new() }
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

        // Suffix array of `src` via SA-IS (see sais.rs). Owned directly by
        // `self.sa` so it is freed/reused here instead of copied around:
        // any correct SA construction yields the same BWT (see module doc).
        self.sa = build_suffix_array(src);
        let sa = &self.sa[..count];

        // One pass over the SA locates the primary row (suffix 0) and the
        // rank of each chunk-start suffix. This replaces a separate
        // inverse-rank array of n u32 (a full allocation, a random-write
        // scatter and a random-read gather per block) -- the same fusion
        // kanzi-cpp's DivSufSort::constructBWT does. The per-element
        // division is hidden behind the streaming SA reads (see
        // OPTIMIZATIONS.md, "constructBWT ... Lemire fastmod").
        let mut p_idx = 0usize; // rank of suffix 0 (primary index, 0-based)
        let mut chunk_ranks = [0u32; 8];

        for (r, &s) in sa.iter().enumerate() {
            let pos = s as usize;

            if pos == 0 {
                p_idx = r;
            }

            let c = pos / step;

            if c < chunks && c * step == pos {
                chunk_ranks[c] = r as u32;
            }
        }

        // BWT payload: [src[n-1]] + predecessors except the primary row.
        let out = &mut dst[header_size..header_size + count];
        out[0] = src[count - 1];
        let mut o = 1usize;

        for (i, &s) in sa.iter().enumerate() {
            if i == p_idx {
                continue;
            }

            // src[(s + count - 1) % count] without the per-element IDIV:
            // s is in [0, count), so only s == 0 wraps to the last byte.
            let pred = if s == 0 { count - 1 } else { s as usize - 1 };
            out[o] = src[pred];
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
            let r = chunk_ranks[c] as usize;
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
            self.primary_indexes[c] = chunk_ranks[c] as usize + 1;
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
        if count > BWT_MERGE_TPSI_MAX {
            return Err("BWT inverse transform failed: block too big (BiPSIv2 not ported, limit is 16 MiB)");
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

        // This is a real bug in kanzi-go itself (reproduced against the
        // real CLI, not just this port), not a missing safeguard Go has
        // and this port dropped: Go's own inverseMergeTPSI sizes its
        // buffer to max(count, 64), matching what was here. The traversal
        // below can, for a *corrupted* stream, dereference the special
        // "wrap to start" sentinel value 0xFF00 as if it were a real
        // linked position -- its low byte is 0xFF (255), a valid `data`
        // index only when count > 255. Both Go and this port panicked
        // with "index out of range [255]" on the same corrupted input
        // before this fix. Sizing the scratch buffer to max(count, 256)
        // (Forward already uses exactly this bound, for the same class of
        // reason) makes that dereference land on an unused, zero-filled
        // slot instead of going out of bounds; the resulting garbage
        // output for a block that small is still expected to fail the
        // container's own checksum/size validation, same as any other
        // corruption this decoder rejects instead of silently accepting.
        let data_len = count.max(256);

        if self.buffer.len() < data_len {
            self.buffer = vec![0i32; data_len];
        } else if count < 256 {
            // `self.buffer` is reused across blocks (this `Bwt` lives for
            // the whole container decode); a *bigger* earlier block can
            // leave stale packed (index, value) entries here whose index
            // exceeds this call's `data_len`, which the fix above alone
            // wouldn't catch. Zero exactly the region beyond `count` that
            // this call itself never (or only via the sentinel) writes,
            // so a stale entry there can't smuggle in an out-of-range
            // link from a previous, larger call.
            self.buffer[count..256].fill(0);
        }

        let data = &mut self.buffer[..data_len];

        // Counting sort into packed (index, value) entries: (i<<8)|val,
        // i up to count-1 < 2^24 (checked above). Read back with a logical
        // shift (see module doc) so the full 2^24 range round-trips
        // correctly regardless of the packed value's sign as an i32.
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
                t = ((ptr as u32) >> 8) as i32;
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
                t[0] = ((ptr0 as u32) >> 8) as i32;
                let ptr1 = data[t[1] as usize];
                d1[n] = ptr1 as u8;
                t[1] = ((ptr1 as u32) >> 8) as i32;
                let ptr2 = data[t[2] as usize];
                d2[n] = ptr2 as u8;
                t[2] = ((ptr2 as u32) >> 8) as i32;
                let ptr3 = data[t[3] as usize];
                d3[n] = ptr3 as u8;
                t[3] = ((ptr3 as u32) >> 8) as i32;
                let ptr4 = data[t[4] as usize];
                d4[n] = ptr4 as u8;
                t[4] = ((ptr4 as u32) >> 8) as i32;
                let ptr5 = data[t[5] as usize];
                d5[n] = ptr5 as u8;
                t[5] = ((ptr5 as u32) >> 8) as i32;
                let ptr6 = data[t[6] as usize];
                d6[n] = ptr6 as u8;
                t[6] = ((ptr6 as u32) >> 8) as i32;
                let ptr7 = data[t[7] as usize];
                d7[n] = ptr7 as u8;
                t[7] = ((ptr7 as u32) >> 8) as i32;
                n += 1;
            }

            while n < ck_size {
                let ptr0 = data[t[0] as usize];
                d0[n] = ptr0 as u8;
                t[0] = ((ptr0 as u32) >> 8) as i32;
                let ptr1 = data[t[1] as usize];
                d1[n] = ptr1 as u8;
                t[1] = ((ptr1 as u32) >> 8) as i32;
                let ptr2 = data[t[2] as usize];
                d2[n] = ptr2 as u8;
                t[2] = ((ptr2 as u32) >> 8) as i32;
                let ptr3 = data[t[3] as usize];
                d3[n] = ptr3 as u8;
                t[3] = ((ptr3 as u32) >> 8) as i32;
                let ptr4 = data[t[4] as usize];
                d4[n] = ptr4 as u8;
                t[4] = ((ptr4 as u32) >> 8) as i32;
                let ptr5 = data[t[5] as usize];
                d5[n] = ptr5 as u8;
                t[5] = ((ptr5 as u32) >> 8) as i32;
                let ptr6 = data[t[6] as usize];
                d6[n] = ptr6 as u8;
                t[6] = ((ptr6 as u32) >> 8) as i32;
                n += 1;
            }
        }

        Ok((count, count))
    }
}
