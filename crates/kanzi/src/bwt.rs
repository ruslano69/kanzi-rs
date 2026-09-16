// Port of kanzi-go's BWT stage for level 5: BWTBlockCodec framing
// (transform/BWTBlockCodec.go) over the BWT core (transform/BWT.go).
//
// Forward suffix-array construction: the BWT output and primary indexes
// derive deterministically from the plain suffix array (which is unique --
// all n suffixes are pairwise distinct strings), so any correct SA
// construction yields byte-identical results. This module picks among
// three interchangeable backends -- see `build_suffix_array` below --
// rather than hand-rolling one itself.
//
// (History: this port originally built the SA with sais.rs, a from-scratch
// SA-IS implementation, specifically to avoid the mistranslation risk of a
// literal DivSufSort port; divsufsort.rs is that DivSufSort port anyway,
// written later once sais.rs's -- and libsais' -- proven-identical output
// gave it a cheap correctness oracle to fuzz against. It is faster than
// sais.rs and needs no C toolchain, so it replaced sais.rs as the default
// here; sais.rs stays in the tree purely as that test oracle, see
// divsufsort.rs's own test module. An earlier version before either used
// prefix-doubling + 2-pass radix sort, O(n log n) and noticeably slower on
// large blocks.)
//
// Inverse ports inverseMergeTPSI exactly (single- and 8-chunk walks,
// sequential -- jobs=1 like this project's single-job container).
// MergeTPSI's own limit is its (index<<8)|value packing into one i32,
// which needs `index` (up to count-1) to fit in 24 bits, i.e. count <=
// 1<<24 (16 MiB). Go's comment on this packing says the same 2^24 bound
// but Go reads it back with an arithmetic (sign-propagating) shift, which
// actually only stays correct up to count <= 1<<23 since Go never
// exercises this path above its own 4MB algorithm-choice threshold; this
// port reads it back with a logical (unsigned) shift instead, so it is
// not bound by that narrower accidental limit and safely spans the full
// 1<<24 the packing scheme was designed for.
//
// inverseBiPSIv2 is also ported (see inverse_bipsiv2/bipsiv2_task_run) --
// kanzi-cpp's own memory-efficient, chunk-independent alternative, used
// here above BIPSI_THRESHOLD (empirically chosen, not kanzi-cpp's own 4MB/
// 2MB thresholds -- see BENCHMARKS.md) both for the algorithmic speedup it
// measures there and because it has no 1<<24 ceiling, so custom block
// sizes above 16 MiB (unreachable via this project's own level 5-7
// defaults, but possible via an explicit CLI block-size argument) now
// decode instead of failing loudly. Only the v6+ block header layout is
// supported (this project is v7-only, and Go takes the same branch for
// versions 6 and 7).
//
// Wire format (BWTBlockCodec, v6+): [mode:1][primary indexes]
// [bwt data], mode = (logChunks<<2)|(pIndexSize-1); the BWT payload itself
// is [src[n-1]] + all rotation-predecessors except the primary row.

use crate::logtables::TAB_LOG2;

/// Suffix-array construction backend for the forward BWT.
///
/// Default: the in-tree `divsufsort.rs` (a port of kanzi-cpp's DivSufSort) --
/// pure Rust, no C toolchain needed, and 1.5-2.7x faster than the older
/// sais.rs backend across the whole Silesia corpus (see BENCHMARKS.md).
/// With the `fast-sa` feature, the libsais C library is used instead for a
/// further ~2x on top of that. Both document the exact same sentinel
/// convention the BWT stage needs ("sorts suffixes as if a unique,
/// lexicographically smallest character were present at the end of the
/// text"), so either is a drop-in replacement yielding the identical SA.
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
    crate::divsufsort::suffix_array(src)
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

// BiPSIv2 constants, ported from kanzi-cpp's BWT.hpp/BWT.cpp verbatim
// (NB_FASTBITS/MASK_FASTBITS name the same values there).
const BIPSI_NB_FASTBITS: u32 = 17;
// Empirically-drawn dispatch threshold between inverse_merge_tpsi and
// inverse_bipsiv2 -- see BENCHMARKS.md's "decode thread pool" section for
// the size x content-type sweep. Below this, BiPSIv2's fixed setup cost
// (a 65536-entry buckets table + a 131072-entry fastBits table, built
// unconditionally regardless of block size) isn't amortized and it loses
// to MergeTPSI, worst-case by ~27% at 4 MiB on incompressible content; at
// and above it, BiPSIv2 is at worst ~2% slower (8 MiB, incompressible) and
// otherwise a clear double-digit-percent win (any content with real
// redundancy, which is the common case for anything reaching BWT at all).
const BIPSI_THRESHOLD: usize = 8 * 1024 * 1024;
const BIPSI_MASK_FASTBITS: usize = (1usize << BIPSI_NB_FASTBITS) - 1;

pub struct Bwt {
    buffer: Vec<i32>,
    primary_indexes: [usize; 8],
    // Suffix-array result (owned across calls so its allocation is reused
    // by the next block handled by the same worker).
    sa: Vec<u32>,
    // BiPSIv2 scratch (see inverse_bipsiv2): a completely separate set of
    // buffers from MergeTPSI's `buffer` above -- different representation
    // (plain u32 "psi" permutation vs MergeTPSI's packed (index<<8)|value
    // i32), reused across calls the same way.
    bipsi_buffer: Vec<u32>,
    bipsi_buckets: Vec<u32>,
    bipsi_fastbits: Vec<u16>,
}

impl Bwt {
    pub fn new() -> Self {
        Bwt {
            buffer: Vec::new(),
            primary_indexes: [0usize; 8],
            sa: Vec::new(),
            bipsi_buffer: Vec::new(),
            bipsi_buckets: Vec::new(),
            bipsi_fastbits: Vec::new(),
        }
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
        // kanzi-cpp's DivSufSort::constructBWT does, but division-free:
        // kanzi-cpp tests `(s % step) == 0` per element (one IDIV per
        // suffix); here chunk starts are only 1 or 8 known values, so an
        // equality check against them avoids the IDIV entirely. Chunk 0
        // always starts at 0, so its rank is p_idx by definition.
        let mut p_idx = 0usize; // rank of suffix 0 (primary index, 0-based)
        let mut chunk_ranks = [0u32; 8];

        if chunks == 1 {
            for (r, &s) in sa.iter().enumerate() {
                if s == 0 {
                    p_idx = r;
                    break;
                }
            }

            chunk_ranks[0] = p_idx as u32;
        } else {
            // chunks == 8 (the only other value get_bwt_chunks returns).
            // All 8 starts are < count here: step = ceil(count/8) and
            // 7*step < count for every count >= 256 (49 < count).
            let s1 = step;
            let s2 = step * 2;
            let s3 = step * 3;
            let s4 = step * 4;
            let s5 = step * 5;
            let s6 = step * 6;
            let s7 = step * 7;
            let mut r1 = 0u32;
            let mut r2 = 0u32;
            let mut r3 = 0u32;
            let mut r4 = 0u32;
            let mut r5 = 0u32;
            let mut r6 = 0u32;
            let mut r7 = 0u32;

            for (r, &s) in sa.iter().enumerate() {
                let pos = s as usize;

                if pos == 0 {
                    p_idx = r;
                } else if pos == s1 {
                    r1 = r as u32;
                } else if pos == s2 {
                    r2 = r as u32;
                } else if pos == s3 {
                    r3 = r as u32;
                } else if pos == s4 {
                    r4 = r as u32;
                } else if pos == s5 {
                    r5 = r as u32;
                } else if pos == s6 {
                    r6 = r as u32;
                } else if pos == s7 {
                    r7 = r as u32;
                }
            }

            chunk_ranks[0] = p_idx as u32;
            chunk_ranks[1] = r1;
            chunk_ranks[2] = r2;
            chunk_ranks[3] = r3;
            chunk_ranks[4] = r4;
            chunk_ranks[5] = r5;
            chunk_ranks[6] = r6;
            chunk_ranks[7] = r7;
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

        let (header_size, block_size) = self.read_inverse_header(src)?;
        let payload = &src[header_size..header_size + block_size];

        // BiPSIv2 above BIPSI_THRESHOLD, MergeTPSI below it -- see
        // BENCHMARKS.md's "decode thread pool" section for the size x
        // content-type sweep this threshold is drawn from. Single-threaded:
        // the same sweep found threading BiPSIv2 itself adds only ~3-8%
        // beyond 1 thread on this machine (memory-bandwidth-, not
        // spawn-cost-, bound past small thread counts), too little to
        // justify wiring spare-thread lending through the decode call
        // stack on top of the larger, already-banked algorithmic win.
        if block_size >= BIPSI_THRESHOLD {
            self.inverse_bipsiv2(payload, dst, block_size, 1)
        } else {
            self.inverse_merge_tpsi(payload, dst, block_size)
        }
    }

    /// [`Bwt::inverse`] with the output overwriting the input: `buf` holds
    /// the forward transform's output (header + payload) and, on success,
    /// exactly the recovered block. Both inverse algorithms read the payload
    /// only while building their tables and never while emitting, so this
    /// needs no separate block-sized output buffer -- on the container's BWT
    /// levels that is one buffer the size of the block less per block in
    /// flight, next to the 4-bytes-per-byte tables themselves.
    pub fn inverse_in_place(&mut self, buf: &mut Vec<u8>) -> Result<usize, &'static str> {
        if buf.is_empty() {
            return Ok(0);
        }

        let (header_size, block_size) = self.read_inverse_header(buf)?;
        let payload = &buf[header_size..header_size + block_size];

        if block_size >= BIPSI_THRESHOLD {
            let tables = self.bipsiv2_build(payload, block_size)?;
            self.bipsiv2_emit(tables, &mut buf[..block_size], block_size, 1);
        } else {
            match self.merge_tpsi_build(payload, block_size)? {
                MergeTpsiStart::Single(b) => buf[0] = b,
                MergeTpsiStart::Chains(p_idx) => self.merge_tpsi_emit(&mut buf[..block_size], block_size, p_idx),
            }
        }

        buf.truncate(block_size);
        Ok(block_size)
    }

    /// Parses the chunk-count / primary-index header in front of a BWT
    /// payload into `self.primary_indexes`; returns (header size, payload
    /// size). `src` must not be empty.
    fn read_inverse_header(&mut self, src: &[u8]) -> Result<(usize, usize), &'static str> {
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

        Ok((header_size, block_size))
    }

    /// Port of BWT.inverseMergeTPSI (sequential, jobs=1).
    fn inverse_merge_tpsi(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        count: usize,
    ) -> Result<(usize, usize), &'static str> {
        if count > dst.len() {
            return Err("BWT inverse transform failed: output buffer too small");
        }

        match self.merge_tpsi_build(src, count)? {
            MergeTpsiStart::Single(b) => dst[0] = b,
            MergeTpsiStart::Chains(p_idx) => self.merge_tpsi_emit(&mut dst[..count], count, p_idx),
        }

        Ok((count, count))
    }

    /// MergeTPSI's table-building half: validates the header and packs
    /// `src` into `self.buffer`. The only half that reads `src`.
    fn merge_tpsi_build(&mut self, src: &[u8], count: usize) -> Result<MergeTpsiStart, &'static str> {
        if count > BWT_MERGE_TPSI_MAX {
            return Err("BWT inverse transform failed: block too big (BiPSIv2 not ported, limit is 16 MiB)");
        }

        if count == 1 {
            return Ok(MergeTpsiStart::Single(src[0]));
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

        if get_bwt_chunks(count) == 8 {
            for &p in &self.primary_indexes[..8] {
                if p == 0 || p > count {
                    return Err("BWT inverse transform failed: corrupted BWT primary index");
                }
            }
        }

        Ok(MergeTpsiStart::Chains(p_idx))
    }

    /// MergeTPSI's emitting half: walks the chains built by
    /// `merge_tpsi_build` into `dst` (exactly `count` bytes).
    fn merge_tpsi_emit(&self, dst: &mut [u8], count: usize, p_idx: usize) {
        let data = &self.buffer[..count.max(256)];

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
                *tk = self.primary_indexes[k] as i32 - 1;
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

    }

    /// Port of kanzi-cpp's `BWT::inverseBiPSIv2` + `InverseBiPSIv2Task::run()`
    /// (`src/transform/BWT.cpp`/`.hpp` in kanzi-cpp, fetched and transcribed
    /// type-for-type: `int`/`uint` loop and count variables that are always
    /// non-negative become `usize`, `uint` buffer/bucket entries become
    /// `u32`, `uint16` become `u16`). Unlike `inverse_merge_tpsi` (an
    /// interleaved chain walk -- splitting its chains across threads only
    /// redistributes one core's memory-level parallelism, not adds to it;
    /// see BENCHMARKS.md), BiPSIv2's 8 chunks are independent by
    /// construction, so `num_threads > 1` here adds genuine cross-core
    /// parallelism. Only ever called with `count` large enough that
    /// `get_bwt_chunks(count) == 8` (kanzi-cpp itself never invokes
    /// BiPSIv2 below its own 2 MiB threshold, where 1-chunk blocks are
    /// possible); the 1-chunk case is `inverse_merge_tpsi`'s alone.
    ///
    /// Correctness rests on differential testing against `inverse_merge_tpsi`
    /// (see the `bipsiv2` test module below and `BENCHMARKS.md`), not on
    /// re-deriving the bidirectional-PSI math from scratch -- the same
    /// stance this project already took for `divsufsort.rs`.
    ///
    /// One deviation from a byte-for-byte transcription: kanzi-cpp's
    /// `InverseBiPSIv2Task::run()` computes its 8 chunk-group destination
    /// pointers (`d0..d7`) as fixed offsets from the *start of the whole
    /// output buffer* and relies on its own `_start`/`_total` fields
    /// (initialized from the task's global `firstChunk * ckSize`) to land
    /// writes in the right place -- safe there because every byte a task
    /// touches is provably confined to its own `[firstChunk, lastChunk)`
    /// chunk range, so C++ can alias one raw buffer across threads with no
    /// synchronization. This port instead hands each task its own
    /// *disjoint* `&mut [u8]` sub-slice (via `split_at_mut`, entirely safe
    /// Rust, no `unsafe`) and rebases every absolute-offset quantity
    /// (`_start`, and `_total` everywhere except the `shift` computation,
    /// which must stay on the true global total since `fast_bits`/`buckets`
    /// are shared, built-once-for-the-whole-block tables) by subtracting
    /// the constant `first_chunk * ck_size` -- a uniform coordinate shift
    /// that cancels out in every difference the algorithm actually
    /// computes (worked through by hand; pinned down empirically by the
    /// same differential suite, run at every thread count 1..=8, since a
    /// rebasing mistake would show up as a thread-count-dependent output
    /// change, which single-threaded-only testing could never catch).
    fn inverse_bipsiv2(
        &mut self,
        src: &[u8],
        dst: &mut [u8],
        count: usize,
        num_threads: usize,
    ) -> Result<(usize, usize), &'static str> {
        if count > dst.len() {
            return Err("BWT inverse transform failed: output buffer too small");
        }

        let tables = self.bipsiv2_build(src, count)?;
        self.bipsiv2_emit(tables, &mut dst[..count], count, num_threads);
        Ok((count, count))
    }

    /// BiPSIv2's setup half: validates the primary indexes and builds the
    /// shared `bipsi_buffer`/`bipsi_buckets`/`bipsi_fastbits` tables. The only
    /// half that reads `src`.
    fn bipsiv2_build(&mut self, src: &[u8], count: usize) -> Result<BipsiTables, &'static str> {
        if get_bwt_chunks(count) != 8 {
            return Err("BWT inverse (BiPSIv2) failed: block too small (needs 8 chunks)");
        }

        // Copied out (Copy type) before `self.bipsi_*` are borrowed below,
        // so there is no overlapping-field-borrow conflict.
        let primary_indexes = self.primary_indexes;
        let p_idx = primary_indexes[0];

        if p_idx == 0 || p_idx > count {
            return Err("Invalid input: corrupted BWT primary index");
        }

        for &p in &primary_indexes[1..8] {
            if p == 0 || p > count {
                return Err("Invalid input: corrupted BWT primary index");
            }
        }

        // kanzi-cpp sizes this to `max(count + 1, 256)`, not `count` -- one
        // extra slot beyond `count` is a real, used position (not just
        // defensive padding; a bucket cursor can legitimately land on it),
        // found by this port's own differential tests panicking on the
        // smallest BiPSIv2-eligible size (256) before this fix.
        if self.bipsi_buffer.len() < count + 1 {
            self.bipsi_buffer = vec![0u32; (count + 1).max(256)];
        }

        if self.bipsi_buckets.is_empty() {
            self.bipsi_buckets = vec![0u32; 65536];
        } else {
            self.bipsi_buckets.iter_mut().for_each(|b| *b = 0);
        }

        if self.bipsi_fastbits.is_empty() {
            self.bipsi_fastbits = vec![0u16; BIPSI_MASK_FASTBITS + 1];
        } else {
            self.bipsi_fastbits.iter_mut().for_each(|b| *b = 0);
        }

        let data = &mut self.bipsi_buffer[..count + 1];
        let buckets = &mut self.bipsi_buckets;
        let fast_bits = &mut self.bipsi_fastbits;

        // --- Setup phase (always single-threaded, like kanzi-cpp: this
        // builds the shared `buckets`/`fast_bits`/`data` tables every task
        // below only reads (buckets/fast_bits) or walks (data)). ---

        let mut freqs = [0u32; 256];

        for &b in &src[..count] {
            freqs[b as usize] += 1;
        }

        let mut sum = 1usize;

        for c in 0..256usize {
            let f = sum;
            sum += freqs[c] as usize;
            freqs[c] = f as u32;

            if f != sum {
                let hi = sum.min(p_idx);

                for i in f..hi {
                    buckets[(c << 8) | src[i] as usize] += 1;
                }

                let lo = (f - 1).max(p_idx);

                for i in lo..sum - 1 {
                    buckets[(c << 8) | src[i] as usize] += 1;
                }
            }
        }

        let lastc = src[0] as usize;
        let mut shift = 0u32;

        while (count >> shift) > BIPSI_MASK_FASTBITS {
            shift += 1;
        }

        {
            let mut v = 0usize;
            let mut sum2 = 1usize;

            for c in 0..256usize {
                if c == lastc {
                    sum2 += 1;
                }

                for d in 0..256usize {
                    let idx = (d << 8) | c;
                    let s = sum2;
                    sum2 += buckets[idx] as usize;
                    buckets[idx] = s as u32;

                    if s == sum2 {
                        continue;
                    }

                    while v <= (sum2 - 1) >> shift {
                        fast_bits[v] = ((c << 8) | d) as u16;
                        v += 1;
                    }
                }
            }
        }

        // Build the inverse ("psi") permutation into `data`. Note the two
        // loops store *different* things at `n`'s boundary: the first
        // stores the pre-increment `n`, the second the post-increment `n`
        // (`n++` runs before the store there) -- a real asymmetry in the
        // source, not a transcription slip; preserved exactly.
        // kanzi-cpp only memsets the first `count` elements here (not the
        // `count + 1`-sized allocation) -- preserved exactly, since the
        // build-inverse loops below only ever address slot `count` via a
        // bucket cursor write, never a read of pre-existing content there.
        for x in data[..count].iter_mut() {
            *x = 0;
        }

        let mut n = 0usize;

        while n < p_idx {
            let c = src[n] as usize;
            let p = freqs[c] as usize;

            if p < p_idx {
                let slot = (c << 8) | src[p] as usize;
                let b = buckets[slot] as usize;
                buckets[slot] += 1;
                data[b] = n as u32;
            } else if p > p_idx {
                let slot = (c << 8) | src[p - 1] as usize;
                let b = buckets[slot] as usize;
                buckets[slot] += 1;
                data[b] = n as u32;
            }

            freqs[c] += 1;
            n += 1;
        }

        while n < count {
            let c = src[n] as usize;
            let p = freqs[c] as usize;
            freqs[c] += 1;
            n += 1;

            if p < p_idx {
                let slot = (c << 8) | src[p] as usize;
                let b = buckets[slot] as usize;
                buckets[slot] += 1;
                data[b] = n as u32;
            } else if p > p_idx {
                let slot = (c << 8) | src[p - 1] as usize;
                let b = buckets[slot] as usize;
                buckets[slot] += 1;
                data[b] = n as u32;
            }
        }

        for c in 0..256usize {
            for d in 0..c {
                buckets.swap((d << 8) | c, (c << 8) | d);
            }
        }

        Ok(BipsiTables { shift, lastc: lastc as u8 })
    }

    /// BiPSIv2's emitting half: decodes the 8 chunks from the tables built by
    /// `bipsiv2_build` into `dst` (exactly `count` bytes).
    fn bipsiv2_emit(&self, tables: BipsiTables, dst: &mut [u8], count: usize, num_threads: usize) {
        let primary_indexes = self.primary_indexes;
        let data = &self.bipsi_buffer[..count + 1];
        let buckets = &self.bipsi_buckets[..];
        let fast_bits = &self.bipsi_fastbits[..];
        let (shift, lastc) = (tables.shift, tables.lastc as usize);

        // --- Dispatch phase: 8 chunks, split across up to `num_threads`
        // tasks the same way `Global::computeJobsPerTask` does. ---

        let chunks = 8usize;
        let st = count / chunks;
        let ck_size = if chunks * st == count { st } else { st + 1 };
        let threads = num_threads.max(1).min(chunks);
        let jobs_per_task = compute_jobs_per_task(chunks, threads);

        // (first_chunk, last_chunk, byte_len) per task -- byte_len is
        // clamped to what's actually left in `dst` for the last task,
        // since `chunks * ck_size` can overshoot `count` by up to
        // `ck_size - 1` bytes (only the globally-last chunk is ever
        // shorter than `ck_size`; `run()`'s `end8`/`end4`/`end2` clamps
        // are exactly what handles that internally).
        let mut task_ranges: Vec<(usize, usize, usize)> = Vec::with_capacity(threads);
        let mut c = 0usize;

        for &jobs in &jobs_per_task {
            let first_chunk = c;
            let last_chunk = c + jobs;
            let byte_len = if last_chunk == chunks {
                count - first_chunk * ck_size
            } else {
                jobs * ck_size
            };
            task_ranges.push((first_chunk, last_chunk, byte_len));
            c = last_chunk;
        }

        if threads == 1 {
            bipsiv2_task_run(data, buckets, fast_bits, dst, &primary_indexes, shift, count, ck_size, 0, chunks);
        } else {
            let mut remaining = &mut dst[..];
            let mut slices: Vec<&mut [u8]> = Vec::with_capacity(threads);

            for &(_, _, byte_len) in &task_ranges {
                let (head, tail) = remaining.split_at_mut(byte_len);
                slices.push(head);
                remaining = tail;
            }

            let (data_ref, buckets_ref, fast_bits_ref) = (data, buckets, fast_bits);
            let primary_indexes_ref = &primary_indexes;

            std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(threads);

                for (j, dst_local) in slices.into_iter().enumerate() {
                    let (first_chunk, last_chunk, _) = task_ranges[j];
                    // Global-relative, matching kanzi-cpp's own `_total`
                    // exactly (NOT `dst_local.len()` -- tried that first;
                    // it silently mis-detects every non-last task's own
                    // final chunk as the special short-last-chunk case,
                    // dropping that chunk's genuinely-needed last byte.
                    // `_total`'s only job is deciding whether a chunk is
                    // the globally-short one; the resulting one-byte
                    // overlap store this enables for odd `ck_size` is
                    // handled separately below, at the write site).
                    let total_local = count - first_chunk * ck_size;

                    handles.push(scope.spawn(move || {
                        bipsiv2_task_run(
                            data_ref,
                            buckets_ref,
                            fast_bits_ref,
                            dst_local,
                            primary_indexes_ref,
                            shift,
                            total_local,
                            ck_size,
                            first_chunk,
                            last_chunk,
                        );
                    }));
                }

                for h in handles {
                    h.join().expect("BiPSIv2 worker thread panicked");
                }
            });
        }

        dst[count - 1] = lastc as u8;

    }
}

/// What `Bwt::merge_tpsi_build` leaves for the emitting half.
enum MergeTpsiStart {
    /// A one-byte block is its own inverse.
    Single(u8),
    /// Walk the chains from this (1-based) primary index.
    Chains(usize),
}

/// What `Bwt::bipsiv2_build` leaves for the emitting half, besides the tables
/// stored on `Bwt` itself.
struct BipsiTables {
    shift: u32,
    lastc: u8,
}

/// Port of kanzi-cpp's `Global::computeJobsPerTask(jobsPerTask, jobs,
/// tasks)`: splits `jobs` (always 8 here, BiPSIv2's fixed chunk count) as
/// evenly as possible across `tasks`, front-loading the remainder onto the
/// first `jobs % tasks` tasks.
fn compute_jobs_per_task(jobs: usize, tasks: usize) -> Vec<usize> {
    let (q, mut r) = if jobs <= tasks { (1, 0) } else { (jobs / tasks, jobs % tasks) };
    let mut out = vec![q; tasks];
    let mut n = 0;

    while r != 0 {
        out[n] += 1;
        r -= 1;
        n += 1;
    }

    out
}

/// Port of kanzi-cpp's `DECODE_BWT(P, S)` macro: `S = fastBits[P>>shift]`,
/// then linear-scan forward while `buckets[S] <= P` (a `do..while` in C++,
/// behaviorally identical to this `while` since the loop body runs zero or
/// more times either way once the initial lookup is in hand).
#[inline(always)]
fn bipsiv2_decode(p: usize, shift: u32, fast_bits: &[u16], buckets: &[u32]) -> u16 {
    let mut s = fast_bits[p >> shift];

    while (buckets[s as usize] as usize) <= p {
        s += 1;
    }

    s
}

/// Port of kanzi-cpp's `InverseBiPSIv2Task<T>::run()`. `dst_local` is this
/// task's own disjoint slice of the output (see `inverse_bipsiv2`'s doc
/// comment for the rebasing this required); `total_local` is the global
/// block size minus `first_chunk * ck_size`; `shift` is precomputed once
/// (globally, not rebased) by the caller since it is identical for every
/// task. `primary_indexes` and `data` stay fully global/unrebased --
/// they're shared read-only (primary_indexes) or walked at global
/// positions the permutation itself defines (data).
// The final `p7 = data[p7]` at the end of the 8-way pair-loop's body is a
// literal port of kanzi-cpp's own last statement there; since `chunks` is
// always exactly 8, that loop's `while c + 8 <= last_chunk` can only ever
// run one round, making this specific reassignment dead on its last (only)
// iteration in both the original and this port -- kept for fidelity rather
// than special-cased away.
#[allow(clippy::too_many_arguments, unused_assignments)]
fn bipsiv2_task_run(
    data: &[u32],
    buckets: &[u32],
    fast_bits: &[u16],
    dst_local: &mut [u8],
    primary_indexes: &[usize; 8],
    shift: u32,
    total_local: usize,
    ck_size: usize,
    first_chunk: usize,
    last_chunk: usize,
) {
    let mut c = first_chunk;
    let mut start_local = 0usize;

    // 8-way interleaved path: only ever taken by a task that owns all 8
    // chunks (nbTasks == 1), since chunks is always exactly 8 -- any task
    // owning fewer than 8 can never satisfy `c + 8 <= last_chunk`.
    if start_local + 7 * ck_size <= total_local {
        while c + 8 <= last_chunk {
            let end = start_local + ck_size - 1;
            let end8 = end.min(total_local - 7 * ck_size - 1);
            let mut p0 = primary_indexes[c];
            let mut p1 = primary_indexes[c + 1];
            let mut p2 = primary_indexes[c + 2];
            let mut p3 = primary_indexes[c + 3];
            let mut p4 = primary_indexes[c + 4];
            let mut p5 = primary_indexes[c + 5];
            let mut p6 = primary_indexes[c + 6];
            let mut p7 = primary_indexes[c + 7];

            let mut i = start_local + 1;

            while i <= end8 {
                let mut s0 = fast_bits[p0 >> shift];
                let mut s1 = fast_bits[p1 >> shift];
                let mut s2 = fast_bits[p2 >> shift];
                let mut s3 = fast_bits[p3 >> shift];
                let mut s4 = fast_bits[p4 >> shift];
                let mut s5 = fast_bits[p5 >> shift];
                let mut s6 = fast_bits[p6 >> shift];
                let mut s7 = fast_bits[p7 >> shift];

                while (buckets[s0 as usize] as usize) <= p0 {
                    s0 += 1;
                }
                while (buckets[s1 as usize] as usize) <= p1 {
                    s1 += 1;
                }
                while (buckets[s2 as usize] as usize) <= p2 {
                    s2 += 1;
                }
                while (buckets[s3 as usize] as usize) <= p3 {
                    s3 += 1;
                }
                while (buckets[s4 as usize] as usize) <= p4 {
                    s4 += 1;
                }
                while (buckets[s5 as usize] as usize) <= p5 {
                    s5 += 1;
                }
                while (buckets[s6 as usize] as usize) <= p6 {
                    s6 += 1;
                }
                while (buckets[s7 as usize] as usize) <= p7 {
                    s7 += 1;
                }

                dst_local[0 * ck_size + i - 1] = (s0 >> 8) as u8;
                dst_local[0 * ck_size + i] = s0 as u8;
                dst_local[1 * ck_size + i - 1] = (s1 >> 8) as u8;
                dst_local[1 * ck_size + i] = s1 as u8;
                dst_local[2 * ck_size + i - 1] = (s2 >> 8) as u8;
                dst_local[2 * ck_size + i] = s2 as u8;
                dst_local[3 * ck_size + i - 1] = (s3 >> 8) as u8;
                dst_local[3 * ck_size + i] = s3 as u8;
                dst_local[4 * ck_size + i - 1] = (s4 >> 8) as u8;
                dst_local[4 * ck_size + i] = s4 as u8;
                dst_local[5 * ck_size + i - 1] = (s5 >> 8) as u8;
                dst_local[5 * ck_size + i] = s5 as u8;
                dst_local[6 * ck_size + i - 1] = (s6 >> 8) as u8;
                dst_local[6 * ck_size + i] = s6 as u8;
                dst_local[7 * ck_size + i - 1] = (s7 >> 8) as u8;
                dst_local[7 * ck_size + i] = s7 as u8;

                p0 = data[p0] as usize;
                p1 = data[p1] as usize;
                p2 = data[p2] as usize;
                p3 = data[p3] as usize;
                p4 = data[p4] as usize;
                p5 = data[p5] as usize;
                p6 = data[p6] as usize;
                p7 = data[p7] as usize;

                i += 2;
            }

            // Keep the eighth chain within its logical end. If the common
            // extent is odd, retain the low byte for the first seven chains.
            let odd_common = ((end8 - start_local + 1) & 1) != 0;

            if odd_common {
                let s0 = bipsiv2_decode(p0, shift, fast_bits, buckets);
                let s1 = bipsiv2_decode(p1, shift, fast_bits, buckets);
                let s2 = bipsiv2_decode(p2, shift, fast_bits, buckets);
                let s3 = bipsiv2_decode(p3, shift, fast_bits, buckets);
                let s4 = bipsiv2_decode(p4, shift, fast_bits, buckets);
                let s5 = bipsiv2_decode(p5, shift, fast_bits, buckets);
                let s6 = bipsiv2_decode(p6, shift, fast_bits, buckets);
                let s7 = bipsiv2_decode(p7, shift, fast_bits, buckets);

                dst_local[0 * ck_size + end8] = (s0 >> 8) as u8;
                dst_local[1 * ck_size + end8] = (s1 >> 8) as u8;
                dst_local[2 * ck_size + end8] = (s2 >> 8) as u8;
                dst_local[3 * ck_size + end8] = (s3 >> 8) as u8;
                dst_local[4 * ck_size + end8] = (s4 >> 8) as u8;
                dst_local[5 * ck_size + end8] = (s5 >> 8) as u8;
                dst_local[6 * ck_size + end8] = (s6 >> 8) as u8;
                dst_local[7 * ck_size + end8] = (s7 >> 8) as u8;

                if end8 < end {
                    dst_local[0 * ck_size + end8 + 1] = s0 as u8;
                    dst_local[1 * ck_size + end8 + 1] = s1 as u8;
                    dst_local[2 * ck_size + end8 + 1] = s2 as u8;
                    dst_local[3 * ck_size + end8 + 1] = s3 as u8;
                    dst_local[4 * ck_size + end8 + 1] = s4 as u8;
                    dst_local[5 * ck_size + end8 + 1] = s5 as u8;
                    dst_local[6 * ck_size + end8 + 1] = s6 as u8;
                }

                p0 = data[p0] as usize;
                p1 = data[p1] as usize;
                p2 = data[p2] as usize;
                p3 = data[p3] as usize;
                p4 = data[p4] as usize;
                p5 = data[p5] as usize;
                p6 = data[p6] as usize;
                p7 = data[p7] as usize;
            }

            // The last chunk can be shorter than the other seven. Finish
            // the common extent of the first seven chains without a
            // per-iteration boundary check for the (possibly-shorter)
            // eighth chain, which was already fully written above.
            if end8 < end {
                let next_pos = end8 + if odd_common { 2 } else { 1 };
                let tail_start = next_pos + 1;
                let mut i = tail_start;

                while i <= end {
                    let s0 = bipsiv2_decode(p0, shift, fast_bits, buckets);
                    let s1 = bipsiv2_decode(p1, shift, fast_bits, buckets);
                    let s2 = bipsiv2_decode(p2, shift, fast_bits, buckets);
                    let s3 = bipsiv2_decode(p3, shift, fast_bits, buckets);
                    let s4 = bipsiv2_decode(p4, shift, fast_bits, buckets);
                    let s5 = bipsiv2_decode(p5, shift, fast_bits, buckets);
                    let s6 = bipsiv2_decode(p6, shift, fast_bits, buckets);

                    dst_local[0 * ck_size + i - 1] = (s0 >> 8) as u8;
                    dst_local[0 * ck_size + i] = s0 as u8;
                    dst_local[1 * ck_size + i - 1] = (s1 >> 8) as u8;
                    dst_local[1 * ck_size + i] = s1 as u8;
                    dst_local[2 * ck_size + i - 1] = (s2 >> 8) as u8;
                    dst_local[2 * ck_size + i] = s2 as u8;
                    dst_local[3 * ck_size + i - 1] = (s3 >> 8) as u8;
                    dst_local[3 * ck_size + i] = s3 as u8;
                    dst_local[4 * ck_size + i - 1] = (s4 >> 8) as u8;
                    dst_local[4 * ck_size + i] = s4 as u8;
                    dst_local[5 * ck_size + i - 1] = (s5 >> 8) as u8;
                    dst_local[5 * ck_size + i] = s5 as u8;
                    dst_local[6 * ck_size + i - 1] = (s6 >> 8) as u8;
                    dst_local[6 * ck_size + i] = s6 as u8;

                    p0 = data[p0] as usize;
                    p1 = data[p1] as usize;
                    p2 = data[p2] as usize;
                    p3 = data[p3] as usize;
                    p4 = data[p4] as usize;
                    p5 = data[p5] as usize;
                    p6 = data[p6] as usize;

                    i += 2;
                }

                if next_pos <= end && (((end - next_pos + 1) & 1) != 0) {
                    let s0 = bipsiv2_decode(p0, shift, fast_bits, buckets);
                    let s1 = bipsiv2_decode(p1, shift, fast_bits, buckets);
                    let s2 = bipsiv2_decode(p2, shift, fast_bits, buckets);
                    let s3 = bipsiv2_decode(p3, shift, fast_bits, buckets);
                    let s4 = bipsiv2_decode(p4, shift, fast_bits, buckets);
                    let s5 = bipsiv2_decode(p5, shift, fast_bits, buckets);
                    let s6 = bipsiv2_decode(p6, shift, fast_bits, buckets);

                    dst_local[0 * ck_size + end] = (s0 >> 8) as u8;
                    dst_local[1 * ck_size + end] = (s1 >> 8) as u8;
                    dst_local[2 * ck_size + end] = (s2 >> 8) as u8;
                    dst_local[3 * ck_size + end] = (s3 >> 8) as u8;
                    dst_local[4 * ck_size + end] = (s4 >> 8) as u8;
                    dst_local[5 * ck_size + end] = (s5 >> 8) as u8;
                    dst_local[6 * ck_size + end] = (s6 >> 8) as u8;
                }
            }

            start_local += 8 * ck_size;
            c += 8;
        }
    }

    // 4-way path: a task owning exactly 4 (or a multiple of 4, though
    // chunks == 8 always means at most one pass) chunks, only when
    // `ck_size` is even (kanzi-cpp's own guard -- an odd `ck_size` falls
    // through to the fully-sequential tail loop below instead).
    if (start_local + 3 * ck_size <= total_local) && ((ck_size & 1) == 0) {
        while c + 4 <= last_chunk {
            let end = start_local + ck_size - 1;
            let end4 = end.min(total_local - 3 * ck_size - 1);
            let mut p0 = primary_indexes[c];
            let mut p1 = primary_indexes[c + 1];
            let mut p2 = primary_indexes[c + 2];
            let mut p3 = primary_indexes[c + 3];

            let mut i = start_local + 1;

            while i <= end4 {
                let s0 = bipsiv2_decode(p0, shift, fast_bits, buckets);
                let s1 = bipsiv2_decode(p1, shift, fast_bits, buckets);
                let s2 = bipsiv2_decode(p2, shift, fast_bits, buckets);
                let s3 = bipsiv2_decode(p3, shift, fast_bits, buckets);

                dst_local[0 * ck_size + i - 1] = (s0 >> 8) as u8;
                dst_local[0 * ck_size + i] = s0 as u8;
                dst_local[1 * ck_size + i - 1] = (s1 >> 8) as u8;
                dst_local[1 * ck_size + i] = s1 as u8;
                dst_local[2 * ck_size + i - 1] = (s2 >> 8) as u8;
                dst_local[2 * ck_size + i] = s2 as u8;
                dst_local[3 * ck_size + i - 1] = (s3 >> 8) as u8;
                dst_local[3 * ck_size + i] = s3 as u8;

                p0 = data[p0] as usize;
                p1 = data[p1] as usize;
                p2 = data[p2] as usize;
                p3 = data[p3] as usize;

                i += 2;
            }

            if end4 < end {
                let tail_start = end4 + 1 + (end4 & 1);
                let mut i = tail_start;

                while i <= end {
                    let s0 = bipsiv2_decode(p0, shift, fast_bits, buckets);
                    let s1 = bipsiv2_decode(p1, shift, fast_bits, buckets);
                    let s2 = bipsiv2_decode(p2, shift, fast_bits, buckets);

                    dst_local[0 * ck_size + i - 1] = (s0 >> 8) as u8;
                    dst_local[0 * ck_size + i] = s0 as u8;
                    dst_local[1 * ck_size + i - 1] = (s1 >> 8) as u8;
                    dst_local[1 * ck_size + i] = s1 as u8;
                    dst_local[2 * ck_size + i - 1] = (s2 >> 8) as u8;
                    dst_local[2 * ck_size + i] = s2 as u8;

                    p0 = data[p0] as usize;
                    p1 = data[p1] as usize;
                    p2 = data[p2] as usize;

                    i += 2;
                }
            }

            start_local += 4 * ck_size;
            c += 4;
        }
    }

    // 2-way path: same shape, a task owning exactly 2 chunks with an even
    // `ck_size`.
    if (start_local + ck_size <= total_local) && ((ck_size & 1) == 0) {
        while c + 2 <= last_chunk {
            let end = start_local + ck_size - 1;
            let end2 = end.min(total_local - ck_size - 1);
            let mut p0 = primary_indexes[c];
            let mut p1 = primary_indexes[c + 1];

            let mut i = start_local + 1;

            while i <= end2 {
                let s0 = bipsiv2_decode(p0, shift, fast_bits, buckets);
                let s1 = bipsiv2_decode(p1, shift, fast_bits, buckets);

                dst_local[0 * ck_size + i - 1] = (s0 >> 8) as u8;
                dst_local[0 * ck_size + i] = s0 as u8;
                dst_local[1 * ck_size + i - 1] = (s1 >> 8) as u8;
                dst_local[1 * ck_size + i] = s1 as u8;

                p0 = data[p0] as usize;
                p1 = data[p1] as usize;

                i += 2;
            }

            if end2 < end {
                let tail_start = end2 + 1 + (end2 & 1);
                let mut i = tail_start;

                while i <= end {
                    let s0 = bipsiv2_decode(p0, shift, fast_bits, buckets);

                    dst_local[0 * ck_size + i - 1] = (s0 >> 8) as u8;
                    dst_local[0 * ck_size + i] = s0 as u8;

                    p0 = data[p0] as usize;

                    i += 2;
                }
            }

            start_local += 2 * ck_size;
            c += 2;
        }
    }

    // Fully sequential tail: whatever chunks weren't claimed by an
    // 8/4/2-way pass above (any task with an odd `ck_size`, or fewer than
    // 2 remaining chunks, ends up here for all of its chunks).
    while c < last_chunk {
        let end = (start_local + ck_size).min(total_local - 1);
        let mut p = primary_indexes[c];
        let mut i = start_local + 1;

        while i <= end {
            let mut s = fast_bits[p >> shift];

            while (buckets[s as usize] as usize) <= p {
                s += 1;
            }

            dst_local[i - 1] = (s >> 8) as u8;

            // kanzi-cpp's `end` formula (`_start + _ckSize`, no `- 1`) is a
            // deliberate one-byte overshoot for a non-last, odd-`ck_size`
            // chunk: the pair-stepped loop needs `i` to reach that position
            // to also write the chunk's genuinely-needed final byte at
            // `i - 1` above, and the overshot `dst[i]` write itself is
            // harmless in kanzi-cpp's single shared buffer -- the next
            // chunk's own first iteration (`i = new_start + 1`, writing
            // `dst[i - 1] = dst[end]`) always overwrites it with the
            // correct value. This port hands each task a disjoint slice,
            // so there is no "next chunk" to overwrite it when this
            // overshoot lands on a *non-last* task's own final chunk --
            // skip it there instead of writing (or panicking) past
            // `dst_local`'s real bound. Found by this port's own
            // differential tests (odd `ck_size`, `threads > 1`): the
            // overshoot is otherwise silently written into (or panics
            // against) the next task's disjoint region.
            if i < dst_local.len() {
                dst_local[i] = s as u8;
            }

            p = data[p] as usize;

            i += 2;
        }

        start_local = end;
        c += 1;
    }
}

#[cfg(test)]
mod bipsiv2_tests {
    use super::*;

    fn make_repetitive(n: usize, period: usize) -> Vec<u8> {
        (0..n).map(|i| (i % period) as u8).collect()
    }

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn make_random(n: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        (0..n).map(|_| xorshift(&mut state) as u8).collect()
    }

    fn make_text_like(n: usize, seed: u64) -> Vec<u8> {
        let words: &[&[u8]] = &[b"the", b"quick", b"brown", b"fox", b"jumps", b"over", b"lazy", b"dog", b"a", b"of"];
        let mut state = seed | 1;
        let mut out = Vec::with_capacity(n + 16);

        while out.len() < n {
            let w = words[(xorshift(&mut state) as usize) % words.len()];
            out.extend_from_slice(w);
            out.push(b' ');
        }

        out.truncate(n);
        out
    }

    /// Encodes `data` once (real `forward()`, real primary indexes), then
    /// checks that `inverse_merge_tpsi` and `inverse_bipsiv2` -- at every
    /// thread count 1..=8, exercising every distinct `computeJobsPerTask`
    /// split -- all agree with each other and reproduce `data` exactly.
    fn check_agrees(data: &[u8]) {
        let mut enc = Bwt::new();
        let mut payload = vec![0u8; max_encoded_len(data.len())];
        let (_, written) = enc.forward(data, &mut payload).expect("forward failed");
        payload.truncate(written);

        // Parse the header exactly as `inverse()` does.
        let mode = payload[0];
        let log_nb_chunks = ((mode >> 2) & 0x07) as usize;
        let p_index_size = (mode & 0x03) as usize + 1;
        let chunks = 1usize << log_nb_chunks;
        let header_size = chunks * p_index_size + 1;
        assert_eq!(chunks, 8, "test payload must be large enough for 8 chunks (>= 256 bytes)");
        let block_size = payload.len() - header_size;
        let bwt_payload = &payload[header_size..header_size + block_size];

        let mut primary_indexes = [0usize; 8];
        let mut idx = 1usize;

        for pi in primary_indexes.iter_mut() {
            let shift0 = (p_index_size - 1) << 3;
            let mut primary_index = 0usize;
            let mut shift = shift0;

            loop {
                primary_index = (primary_index << 8) | payload[idx] as usize;
                idx += 1;

                if shift == 0 {
                    break;
                }

                shift -= 8;
            }

            *pi = primary_index + 1;
        }

        let mut merge_out = vec![0u8; block_size];
        {
            let mut d = Bwt::new();
            d.primary_indexes = primary_indexes;
            d.inverse_merge_tpsi(bwt_payload, &mut merge_out, block_size)
                .expect("MergeTPSI inverse failed");
        }
        assert_eq!(&merge_out[..], data, "MergeTPSI itself mismatched the original input (test bug, not BiPSIv2)");

        for threads in 1..=8usize {
            let mut bipsi_out = vec![0u8; block_size];
            let mut d = Bwt::new();
            d.primary_indexes = primary_indexes;
            d.inverse_bipsiv2(bwt_payload, &mut bipsi_out, block_size, threads)
                .unwrap_or_else(|e| panic!("BiPSIv2 (threads={}) failed on {} bytes: {}", threads, data.len(), e));
            assert_eq!(
                &bipsi_out[..],
                &merge_out[..],
                "BiPSIv2 (threads={}) mismatched MergeTPSI on {} bytes",
                threads,
                data.len()
            );
        }
    }

    #[test]
    fn agrees_with_merge_tpsi_various_sizes_and_content() {
        // Sizes deliberately span both parities of ck_size = ceil(n/8):
        // n=2048 -> ck_size=256 (even); n=2001 -> ck_size=251 (odd, so the
        // 4-way/2-way paths are skipped entirely for that size -- see
        // bipsiv2_task_run's guards).
        let sizes = [
            256usize, 257, 300, 1000, 2000, 2001, 2048, 4096, 4097, 10_000, 65_536, 100_000, 131_071, 131_072,
            131_073, 262_144, 1_000_003, 4_000_001, 8_388_608,
        ];

        for &n in &sizes {
            check_agrees(&make_repetitive(n, 7));
            check_agrees(&make_repetitive(n, 251));
            check_agrees(&make_random(n, 0x1234_5678_9abc_def0 ^ n as u64));
            check_agrees(&make_text_like(n, 0xdead_beef_1234_5678 ^ n as u64));
        }
    }

    #[test]
    fn agrees_with_merge_tpsi_all_same_byte() {
        for &n in &[256usize, 2000, 2048, 65_536] {
            check_agrees(&vec![0x42u8; n]);
        }
    }

    #[test]
    fn agrees_with_merge_tpsi_two_symbol_alphabet() {
        // Stresses buckets/fastBits construction with an extreme,
        // near-degenerate symbol distribution (only 2 of 256 buckets rows
        // ever populated).
        for &n in &[256usize, 2001, 65_536] {
            let data: Vec<u8> = (0..n).map(|i| if i % 3 == 0 { 0xAAu8 } else { 0x55u8 }).collect();
            check_agrees(&data);
        }
    }

    /// Not a correctness test -- wall-clock comparison of sequential
    /// MergeTPSI against parallel BiPSIv2 at several thread counts, on one
    /// large block. `#[ignore]`d so normal `cargo test` runs stay fast;
    /// run explicitly with `cargo test --release bench_bipsiv2 --
    /// --ignored --nocapture`. See BENCHMARKS.md's "decode thread pool"
    /// section for the numbers this produced and what was decided from them.
    /// `inverse_in_place` must reproduce `inverse` exactly -- output and
    /// errors -- for both algorithms: 1-chunk and 8-chunk MergeTPSI, and
    /// BiPSIv2 from its threshold up.
    #[test]
    fn inverse_in_place_matches_inverse() {
        let sizes = [
            2usize,
            3,
            255,
            256,
            4099,
            1 << 20,
            BIPSI_THRESHOLD - 1,
            BIPSI_THRESHOLD,
            BIPSI_THRESHOLD + 13,
        ];

        for (k, &n) in sizes.iter().enumerate() {
            let data = if k % 2 == 0 { make_text_like(n, 7 + n as u64) } else { make_random(n, 11 + n as u64) };
            let mut encoded = vec![0u8; max_encoded_len(n)];
            let (_, written) = Bwt::new().forward(&data, &mut encoded).expect("forward failed");
            encoded.truncate(written);

            let mut expected = vec![0u8; n];
            let (_, m) = Bwt::new().inverse(&encoded, &mut expected).expect("inverse failed");
            assert_eq!(&expected[..m], &data[..], "n={n}: inverse mismatch");

            let mut buf = encoded.clone();
            assert_eq!(Bwt::new().inverse_in_place(&mut buf), Ok(n), "n={n}");
            assert!(buf == data, "n={n}: inverse_in_place mismatch");

            // A primary index past the end must fail identically.
            if n > 1 {
                let mut corrupt = encoded.clone();
                let p_index_size = (corrupt[0] & 0x03) as usize + 1;
                corrupt[1..1 + p_index_size].fill(0xFF);
                let mut out = vec![0u8; n];
                let err = Bwt::new().inverse(&corrupt, &mut out).map(|_| ());
                let mut buf = corrupt.clone();
                assert_eq!(Bwt::new().inverse_in_place(&mut buf).map(|_| ()), err, "n={n}: error mismatch");
            }
        }
    }

    #[test]
    #[ignore]
    fn bench_bipsiv2_vs_merge_tpsi() {
        let n = 16 * 1024 * 1024;

        let text_like = {
            let mut state = 0x9e3779b97f4a7c15u64;
            let words: &[&[u8]] = &[b"the", b"quick", b"brown", b"fox", b"jumps", b"over", b"lazy", b"dog", b"kanzi"];
            let mut data = Vec::with_capacity(n);

            while data.len() < n {
                let w = words[(xorshift(&mut state) as usize) % words.len()];
                data.extend_from_slice(w);
                data.push(b' ');
            }

            data.truncate(n);
            data
        };

        let random = make_random(n, 0x1234_5678_9abc_def0);

        let repetitive = {
            let mut chunk = vec![0u8; 65536];
            let mut state = 0xdead_beefu64;
            for b in chunk.iter_mut() {
                *b = xorshift(&mut state) as u8;
            }
            chunk.iter().cloned().cycle().take(n).collect::<Vec<u8>>()
        };

        for (label, data) in [("text-like", &text_like), ("random", &random), ("repetitive-64k", &repetitive)] {
            println!("=== {} ({} MiB) ===", label, n / (1024 * 1024));
            bench_one(data);
        }
    }

    /// Sweeps block size (single-threaded BiPSIv2 only, since the thread
    /// sweep above showed threading adds little beyond ~2-4 threads and
    /// this machine's memory bandwidth, not spawn cost, is the ceiling) to
    /// find where BiPSIv2 stops winning over MergeTPSI on small blocks --
    /// kanzi-cpp itself only switches above 2 MiB (`BLOCK_SIZE_THRESHOLD2`).
    #[test]
    #[ignore]
    fn bench_bipsiv2_size_sweep() {
        for &n in &[4 * 1024 * 1024usize, 8 * 1024 * 1024, 16 * 1024 * 1024] {
            let random = make_random(n, 0x1234_5678_9abc_def0 ^ n as u64);
            let text_like = {
                let mut state = 0x9e3779b97f4a7c15u64 ^ n as u64;
                let words: &[&[u8]] = &[b"the", b"quick", b"brown", b"fox", b"jumps", b"over", b"lazy", b"dog"];
                let mut data = Vec::with_capacity(n);
                while data.len() < n {
                    let w = words[(xorshift(&mut state) as usize) % words.len()];
                    data.extend_from_slice(w);
                    data.push(b' ');
                }
                data.truncate(n);
                data
            };

            for (label, data) in [("random", &random), ("text-like", &text_like)] {
                println!("=== {} KiB ({}) ===", n / 1024, label);
                bench_one_at(data, &[1]);
            }
        }
    }

    fn bench_one(data: &[u8]) {
        bench_one_at(data, &[1, 2, 4, 6, 8]);
    }

    fn bench_one_at(data: &[u8], thread_counts: &[usize]) {
        let mut enc = Bwt::new();
        let mut payload = vec![0u8; max_encoded_len(data.len())];
        let (_, written) = enc.forward(data, &mut payload).expect("forward failed");
        payload.truncate(written);

        let mode = payload[0];
        let log_nb_chunks = ((mode >> 2) & 0x07) as usize;
        let p_index_size = (mode & 0x03) as usize + 1;
        let chunks = 1usize << log_nb_chunks;
        let header_size = chunks * p_index_size + 1;
        let block_size = payload.len() - header_size;
        let bwt_payload = &payload[header_size..header_size + block_size];

        let mut primary_indexes = [0usize; 8];
        let mut idx = 1usize;

        for pi in primary_indexes.iter_mut() {
            let shift0 = (p_index_size - 1) << 3;
            let mut primary_index = 0usize;
            let mut shift = shift0;

            loop {
                primary_index = (primary_index << 8) | payload[idx] as usize;
                idx += 1;

                if shift == 0 {
                    break;
                }

                shift -= 8;
            }

            *pi = primary_index + 1;
        }

        let reps = 10;
        let mut out = vec![0u8; block_size];

        let mut merge_times = Vec::with_capacity(reps);
        for _ in 0..reps {
            let mut d = Bwt::new();
            d.primary_indexes = primary_indexes;
            let t0 = std::time::Instant::now();
            d.inverse_merge_tpsi(bwt_payload, &mut out, block_size).expect("mergeTPSI failed");
            merge_times.push(t0.elapsed());
        }
        merge_times.sort();

        println!("MergeTPSI (1 thread): min={:?} median={:?}", merge_times[0], merge_times[reps / 2]);

        for &threads in thread_counts {
            let mut times = Vec::with_capacity(reps);
            for _ in 0..reps {
                let mut d = Bwt::new();
                d.primary_indexes = primary_indexes;
                let t0 = std::time::Instant::now();
                d.inverse_bipsiv2(bwt_payload, &mut out, block_size, threads).expect("BiPSIv2 failed");
                times.push(t0.elapsed());
            }
            times.sort();
            println!("BiPSIv2 ({} threads): min={:?} median={:?}", threads, times[0], times[reps / 2]);
        }
    }
}
