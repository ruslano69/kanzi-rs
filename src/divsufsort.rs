// Port of kanzi-cpp's DivSufSort (src/transform/DivSufSort.cpp/.hpp), Yuta
// Mori's two-stage suffix-sorting algorithm: a bucket-sort classification of
// type A/B/B* suffixes (`sort_type_bstar`), followed by `ss_sort`'s
// block-based sub-string sort of the type B* substrings and `tr_sort`'s
// Larsson-Sadakane tandem-repeat rank resolution, finished off by an
// induced-sort placement pass (`construct_suffix_array`/`construct_bwt`).
// This is the suffix-array algorithm kanzi-go and kanzi-cpp both use
// natively for their forward BWT stage.
//
// Semantics match `sais::suffix_array` and the `fast-sa` (libsais) backend
// exactly (see bwt.rs's module doc): "sorts suffixes as if a unique
// lexicographically smallest sentinel were appended after the real text",
// so this is a drop-in third backend for `build_suffix_array` in bwt.rs --
// verified byte-identical against both of them (see the tests below and
// BENCHMARKS.md).
//
// This is ported mechanically from the C++, not restructured, specifically
// so it stays checkable line-by-line against the original: every method
// keeps its original name (snake_case'd) and parameter order, every
// raw-pointer-into-`_sa`/`_buffer` trick becomes an explicit index
// computed the same way, and every `~x` (bitwise NOT) becomes Rust's `!x`
// on `i32` (identical two's-complement semantics: `!x == -x - 1`, same as
// C++'s `~x` on a plain `int`). The one structural change is replacing
// DivSufSort's three heap-allocated `Stack`s and two member bucket arrays
// with a single `Ctx` built fresh per call -- the original `reset()`s all
// of that state on every call anyway, so there is no observable
// difference, and it sidesteps a would-be `unsafe` lifetime dance for no
// benefit.
//
// A note on why this exists next to sais.rs at all: bwt.rs's own doc
// comment already establishes that the suffix order of a block is
// mathematically unique (all n suffixes are pairwise distinct strings), so
// *any* correct SA construction yields byte-identical BWT output --
// SA-IS (sais.rs) was chosen over a DivSufSort port originally for lower
// mistranslation risk on ~2700 lines of pointer/offset-heavy code. This
// module is that DivSufSort port anyway, written after the fact, using
// SA-IS's (and libsais') proven output as a correctness oracle instead of
// trusting a line-by-line reading alone -- see the differential tests at
// the bottom of this file.

/// Computes the suffix array of `text` with standard sentinel semantics: a
/// suffix that is a prefix of another (i.e. would be extended by a virtual
/// character smaller than every real byte) sorts first. Matches
/// `sais::suffix_array`'s contract exactly. Returns a `Vec` of length
/// `text.len()`.
pub fn suffix_array(text: &[u8]) -> Vec<u32> {
    let n = text.len();

    // DivSufSort's algorithm (like the original C library) assumes at
    // least two suffixes to compare; every real caller (kanzi-cpp's
    // BWT::forward, this crate's bwt.rs) special-cases n<2 before ever
    // reaching it, so this wrapper does the same instead of porting past
    // that unchecked assumption.
    if n < 2 {
        return (0..n as u32).collect();
    }

    let mut sa = vec![0i32; n];
    compute_suffix_array(text, &mut sa);
    sa.into_iter().map(|x| x as u32).collect()
}

/// Computes the suffix array of `input` directly into `sa` (length
/// `input.len()`, which must be >= 2). Mirrors `DivSufSort::computeSuffixArray`.
pub fn compute_suffix_array(input: &[u8], sa: &mut [i32]) {
    let length = input.len() as i32;
    let mut ctx = Ctx::new(input, sa);
    let m = ctx.sort_type_bstar(length);
    // m < 0 can only happen for length <= 0, excluded by the n<2 guard above.
    debug_assert!(m >= 0);
    ctx.construct_suffix_array(length, m);
}

/// Computes the BWT of `input` (length >= 2) into `output`, using `bwt` as
/// scratch space for the suffix array (length `input.len()`) and writing
/// `idx_count` evenly spaced primary-index checkpoints into `indexes`
/// (`indexes[0]` is always the true primary index + 1). Mirrors
/// `DivSufSort::computeBWT`. Returns `false` were the underlying
/// `sortTypeBstar`/`constructBWT` to fail (never for length >= 2).
pub fn compute_bwt(
    input: &[u8],
    output: &mut [u8],
    bwt: &mut [i32],
    indexes: &mut [i32],
    idx_count: i32,
) -> bool {
    let length = input.len() as i32;
    let mut ctx = Ctx::new(input, bwt);
    let m = ctx.sort_type_bstar(length);

    if m < 0 {
        return false;
    }

    let p_idx = ctx.construct_bwt(length, m, indexes, idx_count);

    if p_idx < 0 {
        return false;
    }

    output[0] = input[(length - 1) as usize];

    for i in 0..p_idx {
        output[(i + 1) as usize] = ctx.sa[i as usize] as u8;
    }

    for i in (p_idx + 1)..length {
        output[i as usize] = ctx.sa[i as usize] as u8;
    }

    true
}

// ---------------------------------------------------------------------
// Constants (DivSufSort.cpp's static consts)
// ---------------------------------------------------------------------

const SS_INSERTIONSORT_THRESHOLD: i32 = 16;
const SS_BLOCKSIZE: i32 = 8192;
const SS_MISORT_STACKSIZE: usize = 16;
const SS_SMERGE_STACKSIZE: usize = 32;
const TR_STACKSIZE: usize = 64;
const TR_INSERTIONSORT_THRESHOLD: i32 = 16;

static SQQ_TABLE: [i32; 256] = [
    0, 16, 22, 27, 32, 35, 39, 42, 45, 48, 50, 53, 55, 57, 59, 61,
    64, 65, 67, 69, 71, 73, 75, 76, 78, 80, 81, 83, 84, 86, 87, 89,
    90, 91, 93, 94, 96, 97, 98, 99, 101, 102, 103, 104, 106, 107, 108, 109,
    110, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126,
    128, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142,
    143, 144, 144, 145, 146, 147, 148, 149, 150, 150, 151, 152, 153, 154, 155, 155,
    156, 157, 158, 159, 160, 160, 161, 162, 163, 163, 164, 165, 166, 167, 167, 168,
    169, 170, 170, 171, 172, 173, 173, 174, 175, 176, 176, 177, 178, 178, 179, 180,
    181, 181, 182, 183, 183, 184, 185, 185, 186, 187, 187, 188, 189, 189, 190, 191,
    192, 192, 193, 193, 194, 195, 195, 196, 197, 197, 198, 199, 199, 200, 201, 201,
    202, 203, 203, 204, 204, 205, 206, 206, 207, 208, 208, 209, 209, 210, 211, 211,
    212, 212, 213, 214, 214, 215, 215, 216, 217, 217, 218, 218, 219, 219, 220, 221,
    221, 222, 222, 223, 224, 224, 225, 225, 226, 226, 227, 227, 228, 229, 229, 230,
    230, 231, 231, 232, 232, 233, 234, 234, 235, 235, 236, 236, 237, 237, 238, 238,
    239, 240, 240, 241, 241, 242, 242, 243, 243, 244, 244, 245, 245, 246, 246, 247,
    247, 248, 248, 249, 249, 250, 250, 251, 251, 252, 252, 253, 253, 254, 254, 255,
];

static LOG_TABLE: [i32; 256] = [
    -1, 0, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3,
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
    6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
    6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
    6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
    6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
];

// ---------------------------------------------------------------------
// Free helper functions (DivSufSort's inline static/const helpers)
// ---------------------------------------------------------------------

#[inline(always)]
fn get_index(a: i32) -> i32 {
    if a >= 0 { a } else { !a }
}

#[inline(always)]
fn ss_ilg(n: i32) -> i32 {
    if n > 255 { 8 + LOG_TABLE[(n >> 8) as usize] } else { LOG_TABLE[(n & 0xFF) as usize] }
}

#[inline(always)]
fn tr_ilg(n: i32) -> i32 {
    let u = n as u32;

    if (u & 0xFFFF_0000) != 0 {
        if (u & 0xFF00_0000) != 0 {
            24 + LOG_TABLE[((n >> 24) & 0xFF) as usize]
        } else {
            16 + LOG_TABLE[((n >> 16) & 0xFF) as usize]
        }
    } else if (u & 0x0000_FF00) != 0 {
        8 + LOG_TABLE[((n >> 8) & 0xFF) as usize]
    } else {
        LOG_TABLE[(n & 0xFF) as usize]
    }
}

fn ss_isqrt(x: i32) -> i32 {
    if x >= SS_BLOCKSIZE * SS_BLOCKSIZE {
        return SS_BLOCKSIZE;
    }

    let u = x as u32;
    let e = if (u & 0xFFFF_0000) != 0 {
        if (u & 0xFF00_0000) != 0 {
            24 + LOG_TABLE[((x >> 24) & 0xFF) as usize]
        } else {
            16 + LOG_TABLE[((x >> 16) & 0xFF) as usize]
        }
    } else if (u & 0x0000_FF00) != 0 {
        8 + LOG_TABLE[((x >> 8) & 0xFF) as usize]
    } else {
        LOG_TABLE[(x & 0xFF) as usize]
    };

    if e < 8 {
        return SQQ_TABLE[x as usize] >> 4;
    }

    let mut y: i32;

    if e >= 16 {
        y = SQQ_TABLE[(x >> ((e - 6) - (e & 1))) as usize] << ((e >> 1) - 7);

        if e >= 24 {
            y = (y + 1 + x / y) >> 1;
        }

        y = (y + 1 + x / y) >> 1;
    } else {
        y = (SQQ_TABLE[(x >> ((e - 6) - (e & 1))) as usize] >> (7 - (e >> 1))) + 1;
    }

    if x < y * y { y - 1 } else { y }
}

// ---------------------------------------------------------------------
// Stack (a fixed-capacity, pre-allocated stack of 5-int frames)
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct StackElement {
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    e: i32,
}

struct Stack {
    arr: Vec<StackElement>,
    index: usize,
}

impl Stack {
    fn new(size: usize) -> Self {
        Stack { arr: vec![StackElement::default(); size], index: 0 }
    }

    #[inline]
    fn push(&mut self, a: i32, b: i32, c: i32, d: i32, e: i32) {
        self.arr[self.index] = StackElement { a, b, c, d, e };
        self.index += 1;
    }

    #[inline]
    fn pop(&mut self) -> Option<StackElement> {
        if self.index == 0 {
            None
        } else {
            self.index -= 1;
            Some(self.arr[self.index])
        }
    }

    /// Mirrors `Stack::get(idx)->_d = ...`, the only field ever mutated
    /// through `get()` in the original.
    #[inline]
    fn set_d(&mut self, idx: i32, d: i32) {
        self.arr[idx as usize].d = d;
    }

    fn size(&self) -> i32 {
        self.index as i32
    }
}

// ---------------------------------------------------------------------
// TRBudget (trSort's iteration-complexity budget check)
// ---------------------------------------------------------------------

struct TrBudget {
    chance: i32,
    remain: i32,
    inc_val: i32,
    count: i32,
}

impl TrBudget {
    fn new(chance: i32, inc_val: i32) -> Self {
        TrBudget { chance, remain: inc_val, inc_val, count: 0 }
    }

    fn check(&mut self, size: i32) -> bool {
        if size <= self.remain {
            self.remain -= size;
            return true;
        }

        if self.chance == 0 {
            self.count += size;
            return false;
        }

        self.remain += self.inc_val - size;
        self.chance -= 1;
        true
    }
}

// ---------------------------------------------------------------------
// Ctx: the per-call algorithm state (DivSufSort's member fields)
// ---------------------------------------------------------------------

struct Ctx<'a> {
    sa: &'a mut [i32],
    buffer: &'a [u8],
    ss_stack: Stack,
    tr_stack: Stack,
    merge_stack: Stack,
    bucket_a: [i32; 256],
    bucket_b: Box<[i32; 65536]>,
}

impl<'a> Ctx<'a> {
    fn new(buffer: &'a [u8], sa: &'a mut [i32]) -> Self {
        Ctx {
            sa,
            buffer,
            ss_stack: Stack::new(SS_MISORT_STACKSIZE),
            tr_stack: Stack::new(TR_STACKSIZE),
            merge_stack: Stack::new(SS_SMERGE_STACKSIZE),
            bucket_a: [0i32; 256],
            bucket_b: Box::new([0i32; 65536]),
        }
    }

    // -------------------------------------------------------------
    // Small character/rank readers (fold DivSufSort's repeated
    // `p[sapa[_sa[x]]]` / `arr[isad + arr[x]]` pointer-chasing idioms).
    // -------------------------------------------------------------

    /// EXPERIMENTAL (not yet applied crate-wide): unchecked reads for the
    /// handful of accessors that sit in this algorithm's hottest loops,
    /// mirroring sais.rs's own documented "get_unchecked after indices are
    /// proven in range" approach. Safety: every index passed to `sa_at`
    /// here is exactly the same expression the original C++ dereferences
    /// through a raw `int*`/`uint8*` with zero checking of its own -- the
    /// algorithm's own invariants (not this port's) are what keep it in
    /// bounds, the same invariants ~10000+ fuzzed/real-file test runs in
    /// this module's own test suite already exercise without ever tripping
    /// the safe version's bounds check.
    #[inline(always)]
    fn buf_at(&self, i: i32) -> u8 {
        debug_assert!(i >= 0 && (i as usize) < self.buffer.len());
        unsafe { *self.buffer.get_unchecked(i as usize) }
    }

    #[inline(always)]
    fn sa_at(&self, i: i32) -> i32 {
        debug_assert!(i >= 0 && (i as usize) < self.sa.len());
        unsafe { *self.sa.get_unchecked(i as usize) }
    }

    /// `_buffer[idx + _sa[pa + _sa[pos]]]` -- the byte at depth `idx` of
    /// the suffix whose start position is stored at `_sa[pa + _sa[pos]]`.
    #[inline]
    fn ss_char(&self, idx: i32, pa: i32, pos: i32) -> i32 {
        self.buf_at(idx + self.sa_at(pa + self.sa_at(pos))) as i32
    }

    /// `_buffer[idx + _sa[pa + v]]` -- like `ss_char`, but `v` is already a
    /// suffix start position (not an index that itself needs an `_sa` hop).
    #[inline]
    fn ss_char_val(&self, idx: i32, pa: i32, v: i32) -> i32 {
        self.buf_at(idx + self.sa_at(pa + v)) as i32
    }

    /// `_sa[isad + _sa[pos]]` -- the rank at depth `isad` of the suffix
    /// whose start position is stored at `_sa[pos]`.
    #[inline]
    fn tr_char(&self, isad: i32, pos: i32) -> i32 {
        self.sa_at(isad + self.sa_at(pos))
    }

    /// `_sa[isad + v]` -- like `tr_char`, but `v` is already a position.
    #[inline]
    fn tr_char_val(&self, isad: i32, v: i32) -> i32 {
        self.sa_at(isad + v)
    }

    // -------------------------------------------------------------
    // sortTypeBstar / constructSuffixArray / constructBWT
    // -------------------------------------------------------------

    fn sort_type_bstar(&mut self, n: i32) -> i32 {
        let mut m = n;
        let mut c0 = self.buffer[(n - 1) as usize] as i32;

        let mut i = n - 1;
        loop {
            if i < 0 {
                break;
            }

            let mut c1;

            loop {
                c1 = c0;
                self.bucket_a[c1 as usize] += 1;
                i -= 1;

                if i < 0 {
                    break;
                }

                c0 = self.buffer[i as usize] as i32;

                if c0 < c1 {
                    break;
                }
            }

            if i < 0 {
                break;
            }

            self.bucket_b[((c0 << 8) + c1) as usize] += 1;
            m -= 1;
            self.sa[m as usize] = i;
            i -= 1;
            c1 = c0;

            while i >= 0 {
                c0 = self.buffer[i as usize] as i32;

                if c0 > c1 {
                    break;
                }

                self.bucket_b[((c1 << 8) + c0) as usize] += 1;
                c1 = c0;
                i -= 1;
            }
        }

        m = n - m;
        c0 = 0;

        {
            let mut bi = 0i32;
            let mut bj = 0i32;

            while c0 < 256 {
                let t = bi + self.bucket_a[c0 as usize];
                self.bucket_a[c0 as usize] = bi + bj;
                let idx = c0 << 8;
                bi = t + self.bucket_b[(idx + c0) as usize];

                for c1 in (c0 + 1)..256 {
                    bj += self.bucket_b[(idx + c1) as usize];
                    self.bucket_b[(idx + c1) as usize] = bj;
                    bi += self.bucket_b[((c1 << 8) + c0) as usize];
                }

                c0 += 1;
            }
        }

        if m > 0 {
            let pab = n - m;

            for i in (0..=(m - 2)).rev() {
                let t = self.sa[(pab + i) as usize];
                let idx = ((self.buffer[t as usize] as i32) << 8) + self.buffer[(t + 1) as usize] as i32;
                self.bucket_b[idx as usize] -= 1;
                let bidx = self.bucket_b[idx as usize];
                self.sa[bidx as usize] = i;
            }

            let t = self.sa[(pab + m - 1) as usize];
            c0 = ((self.buffer[t as usize] as i32) << 8) + self.buffer[(t + 1) as usize] as i32;
            self.bucket_b[c0 as usize] -= 1;
            let bidx = self.bucket_b[c0 as usize];
            self.sa[bidx as usize] = m - 1;

            let buf_size = n - m - m;
            c0 = 254;
            let mut j = m;

            while j > 0 {
                let idx = c0 << 8;

                for c1 in (c0 + 1..256).rev() {
                    let i = self.bucket_b[(idx + c1) as usize];

                    if j > i + 1 {
                        let last_suffix = self.sa[i as usize] == m - 1;
                        self.ss_sort(pab, i, j, m, buf_size, 2, n, last_suffix);
                    }

                    j = i;
                }

                c0 -= 1;
            }

            // Compute ranks of type B* substrings.
            let mut i = m - 1;
            'ranks: loop {
                if i < 0 {
                    break;
                }

                if self.sa[i as usize] >= 0 {
                    let j = i;

                    loop {
                        let v = self.sa[i as usize];
                        self.sa[(m + v) as usize] = i;
                        i -= 1;

                        if !(i >= 0 && self.sa[i as usize] >= 0) {
                            break;
                        }
                    }

                    self.sa[(i + 1) as usize] = i - j;

                    if i <= 0 {
                        break 'ranks;
                    }
                }

                let j = i;

                loop {
                    self.sa[i as usize] = !self.sa[i as usize];
                    let v = self.sa[i as usize];
                    self.sa[(m + v) as usize] = j;
                    i -= 1;

                    if !(self.sa[i as usize] < 0) {
                        break;
                    }
                }

                let v = self.sa[i as usize];
                self.sa[(m + v) as usize] = j;

                i -= 1;
            }

            // Construct the inverse suffix array of type B* suffixes.
            self.tr_sort(m, 1);

            c0 = self.buffer[(n - 1) as usize] as i32;

            let mut i = n - 1;
            let mut j = m;

            while i >= 0 {
                i -= 1;

                let mut c1 = c0;

                while i >= 0 {
                    c0 = self.buffer[i as usize] as i32;

                    if c0 < c1 {
                        break;
                    }

                    c1 = c0;
                    i -= 1;
                }

                if i >= 0 {
                    let tt = i;
                    i -= 1;

                    let mut c1b = c0;

                    while i >= 0 {
                        c0 = self.buffer[i as usize] as i32;

                        if c0 > c1b {
                            break;
                        }

                        c1b = c0;
                        i -= 1;
                    }

                    j -= 1;
                    let target = self.sa[(m + j) as usize];
                    self.sa[target as usize] = if tt == 0 || tt - i > 1 { tt } else { !tt };
                }
            }

            // Calculate the index of start/end point of each bucket.
            self.bucket_b[65535] = n;
            let mut k = m - 1;

            for c0 in (0..=254i32).rev() {
                let mut i = self.bucket_a[(c0 + 1) as usize] - 1;
                let idx = c0 << 8;

                for c1 in (c0 + 1..256).rev() {
                    let tt = i - self.bucket_b[((c1 << 8) + c0) as usize];
                    self.bucket_b[((c1 << 8) + c0) as usize] = i;
                    i = tt;
                    let j = self.bucket_b[(idx + c1) as usize];

                    while k >= j {
                        self.sa[i as usize] = self.sa[k as usize];
                        i -= 1;
                        k -= 1;
                    }
                }

                self.bucket_b[(idx + c0 + 1) as usize] = i - self.bucket_b[(idx + c0) as usize] + 1;
                self.bucket_b[(idx + c0) as usize] = i;
            }
        }

        m
    }

    fn construct_suffix_array(&mut self, n: i32, m: i32) {
        if m > 0 {
            for c1 in (0..=254i32).rev() {
                let idx = c1 << 8;
                let i = self.bucket_b[(idx + c1 + 1) as usize];
                let mut k = 0i32;
                let mut c2 = -1i32;

                for j in (i..self.bucket_a[(c1 + 1) as usize]).rev() {
                    let mut s = self.sa[j as usize];
                    self.sa[j as usize] = !s;

                    if s <= 0 {
                        continue;
                    }

                    s -= 1;
                    let c0 = self.buffer[s as usize] as i32;

                    if s > 0 && self.buffer[(s - 1) as usize] as i32 > c0 {
                        s = !s;
                    }

                    if c0 != c2 {
                        if c2 >= 0 {
                            self.bucket_b[(idx + c2) as usize] = k;
                        }
                        c2 = c0;
                        k = self.bucket_b[(idx + c2) as usize];
                    }

                    self.sa[k as usize] = s;
                    k -= 1;
                }
            }
        }

        let mut c2 = self.buffer[(n - 1) as usize] as i32;
        let mut k = self.bucket_a[c2 as usize];
        self.sa[k as usize] = if (self.buffer[(n - 2) as usize] as i32) < c2 { !(n - 1) } else { n - 1 };
        k += 1;

        for i in 0..n {
            let mut s = self.sa[i as usize];

            if s <= 0 {
                self.sa[i as usize] = !s;
                continue;
            }

            s -= 1;
            let c0 = self.buffer[s as usize] as i32;

            if s == 0 || (self.buffer[(s - 1) as usize] as i32) < c0 {
                s = !s;
            }

            if c0 != c2 {
                self.bucket_a[c2 as usize] = k;
                c2 = c0;
                k = self.bucket_a[c2 as usize];
            }

            self.sa[k as usize] = s;
            k += 1;
        }
    }

    fn construct_bwt(&mut self, n: i32, m: i32, indexes: &mut [i32], idx_count: i32) -> i32 {
        let mut p_idx = -1i32;
        let st = n / idx_count;
        let step = if idx_count * st == n { st } else { st + 1 };

        if m > 0 {
            for c1 in (0..=254i32).rev() {
                let idx = c1 << 8;
                let i = self.bucket_b[(idx + c1 + 1) as usize];
                let mut k = 0i32;
                let mut c2 = -1i32;

                for j in (i..self.bucket_a[(c1 + 1) as usize]).rev() {
                    let s0 = self.sa[j as usize];

                    if s0 <= 0 {
                        if s0 != 0 {
                            self.sa[j as usize] = !s0;
                        }
                        continue;
                    }

                    let mut s = s0;

                    if (s % step) == 0 {
                        indexes[(s / step) as usize] = j + 1;
                    }

                    s -= 1;
                    let c0 = self.buffer[s as usize] as i32;
                    self.sa[j as usize] = !c0;

                    if s > 0 && (self.buffer[(s - 1) as usize] as i32) > c0 {
                        s = !s;
                    }

                    if c0 != c2 {
                        if c2 >= 0 {
                            self.bucket_b[(idx + c2) as usize] = k;
                        }
                        c2 = c0;
                        k = self.bucket_b[(idx + c2) as usize];
                    }

                    self.sa[k as usize] = s;
                    k -= 1;
                }
            }
        }

        let mut c2 = self.buffer[(n - 1) as usize] as i32;
        let mut k = self.bucket_a[c2 as usize];

        if (self.buffer[(n - 2) as usize] as i32) < c2 {
            if ((n - 1) % step) == 0 {
                indexes[((n - 1) / step) as usize] = n;
            }
            self.sa[k as usize] = !(self.buffer[(n - 2) as usize] as i32);
            k += 1;
        } else {
            self.sa[k as usize] = n - 1;
            k += 1;
        }

        for i in 0..n {
            let s0 = self.sa[i as usize];

            if s0 <= 0 {
                if s0 != 0 {
                    self.sa[i as usize] = !s0;
                } else {
                    p_idx = i;
                }
                continue;
            }

            let mut s = s0;

            if (s % step) == 0 {
                indexes[(s / step) as usize] = i + 1;
            }

            s -= 1;
            let c0 = self.buffer[s as usize] as i32;
            self.sa[i as usize] = c0;

            if c0 != c2 {
                self.bucket_a[c2 as usize] = k;
                c2 = c0;
                k = self.bucket_a[c2 as usize];
            }

            if s > 0 && (self.buffer[(s - 1) as usize] as i32) < c0 {
                if (s % step) == 0 {
                    indexes[(s / step) as usize] = k + 1;
                }
                s = !(self.buffer[(s - 1) as usize] as i32);
            }

            self.sa[k as usize] = s;
            k += 1;
        }

        indexes[0] = p_idx + 1;
        p_idx
    }

    // -------------------------------------------------------------
    // ssSort and the ss* substring-sort family
    // -------------------------------------------------------------

    fn ss_sort(&mut self, pa: i32, first0: i32, last: i32, buf0: i32, buf_size0: i32, depth: i32, n: i32, last_suffix: bool) {
        let mut first = first0;
        let mut buf = buf0;
        let mut buf_size = buf_size0;

        if last_suffix {
            first += 1;
        }

        let mut limit = 0i32;
        let mut middle = last;

        if buf_size < SS_BLOCKSIZE && buf_size < last - first {
            limit = ss_isqrt(last - first);

            if buf_size < limit {
                limit = if limit > SS_BLOCKSIZE { SS_BLOCKSIZE } else { limit };
                middle = last - limit;
                buf = middle;
                buf_size = limit;
            } else {
                limit = 0;
            }
        }

        let mut a = first;
        let mut i = 0i32;

        while middle - a > SS_BLOCKSIZE {
            self.ss_multi_key_intro_sort(pa, a, a + SS_BLOCKSIZE, depth);
            let mut cur_buf_size = last - (a + SS_BLOCKSIZE);
            let cur_buf;

            if cur_buf_size > buf_size {
                cur_buf = a + SS_BLOCKSIZE;
            } else {
                cur_buf_size = buf_size;
                cur_buf = buf;
            }

            let mut k = SS_BLOCKSIZE;
            let mut b = a;
            let mut jj = i;

            while (jj & 1) != 0 {
                self.ss_swap_merge(pa, b - k, b, b + k, cur_buf, cur_buf_size, depth);
                b -= k;
                k <<= 1;
                jj >>= 1;
            }

            a += SS_BLOCKSIZE;
            i += 1;
        }

        self.ss_multi_key_intro_sort(pa, a, middle, depth);

        {
            let mut k = SS_BLOCKSIZE;

            while i != 0 {
                if (i & 1) != 0 {
                    self.ss_swap_merge(pa, a - k, a, middle, buf, buf_size, depth);
                    a -= k;
                }

                k <<= 1;
                i >>= 1;
            }
        }

        if limit != 0 {
            self.ss_multi_key_intro_sort(pa, middle, last, depth);
            self.ss_inplace_merge(pa, first, middle, last, depth);
        }

        if last_suffix {
            let i = self.sa[(first - 1) as usize];
            let p1 = self.sa[(pa + i) as usize];
            let p11 = n - 2;

            let mut a = first;
            while a < last && (self.sa[a as usize] < 0 || self.ss_compare_val(p1, p11, pa + self.sa[a as usize], depth) > 0) {
                self.sa[(a - 1) as usize] = self.sa[a as usize];
                a += 1;
            }

            self.sa[(a - 1) as usize] = i;
        }
    }

    /// `ssCompare(const int s1[], const int s2[], depth)`: `s1`/`s2` each
    /// point at a consecutive `(position, bound)` pair in `_sa`, addressed
    /// here by the index of their first element.
    #[inline]
    fn ss_compare(&self, idx1: i32, idx2: i32, depth: i32) -> i32 {
        let mut u1 = depth + self.sa_at(idx1);
        let mut u2 = depth + self.sa_at(idx2);
        let u1n = self.sa_at(idx1 + 1) + 2;
        let u2n = self.sa_at(idx2 + 1) + 2;

        if u1n - u1 > u2n - u2 {
            while u2 < u2n && self.buf_at(u1) == self.buf_at(u2) {
                u1 += 1;
                u2 += 1;
            }
        } else {
            while u1 < u1n && self.buf_at(u1) == self.buf_at(u2) {
                u1 += 1;
                u2 += 1;
            }
        }

        if u1 < u1n {
            if u2 < u2n { self.buf_at(u1) as i32 - self.buf_at(u2) as i32 } else { 1 }
        } else if u2 < u2n {
            -1
        } else {
            0
        }
    }

    /// `ssCompare(int pa, int pb, int p2, depth)`: `pa`/`pb` are already
    /// suffix positions (not indices), `p2` is an index (like `ss_compare`'s
    /// second argument). Only used once, in `ss_sort`'s last-suffix tail.
    #[inline]
    fn ss_compare_val(&self, pa_val: i32, pb_val: i32, p2_idx: i32, depth: i32) -> i32 {
        let mut u1 = depth + pa_val;
        let mut u2 = depth + self.sa_at(p2_idx);
        let u1n = pb_val + 2;
        let u2n = self.sa_at(p2_idx + 1) + 2;

        if u1n - u1 > u2n - u2 {
            while u2 < u2n && self.buf_at(u1) == self.buf_at(u2) {
                u1 += 1;
                u2 += 1;
            }
        } else {
            while u1 < u1n && self.buf_at(u1) == self.buf_at(u2) {
                u1 += 1;
                u2 += 1;
            }
        }

        if u1 < u1n {
            if u2 < u2n { self.buf_at(u1) as i32 - self.buf_at(u2) as i32 } else { 1 }
        } else if u2 < u2n {
            -1
        } else {
            0
        }
    }

    fn ss_inplace_merge(&mut self, pa: i32, first: i32, middle0: i32, last0: i32, depth: i32) {
        let mut middle = middle0;
        let mut last = last0;

        loop {
            let x;
            let p;

            if self.sa[(last - 1) as usize] < 0 {
                x = 1;
                p = pa + !self.sa[(last - 1) as usize];
            } else {
                x = 0;
                p = pa + self.sa[(last - 1) as usize];
            }

            let mut a = first;
            let mut r = -1i32;

            let mut len = middle - first;
            let mut half = len >> 1;

            while len > 0 {
                let b = a + half;
                let sb = self.sa[b as usize];
                let idx_b = if sb >= 0 { sb } else { !sb };
                let q = self.ss_compare(pa + idx_b, p, depth);

                if q < 0 {
                    a = b + 1;
                    half -= (len & 1) ^ 1;
                } else {
                    r = q;
                }

                len = half;
                half >>= 1;
            }

            if a < middle {
                if r == 0 {
                    self.sa[a as usize] = !self.sa[a as usize];
                }

                self.ss_rotate(a, middle, last);
                last -= middle - a;
                middle = a;

                if first == middle {
                    break;
                }
            }

            last -= 1;

            if x != 0 {
                last -= 1;

                while self.sa[last as usize] < 0 {
                    last -= 1;
                }
            }

            if middle == last {
                break;
            }
        }
    }

    fn ss_rotate(&mut self, first0: i32, middle: i32, last0: i32) {
        let mut first = first0;
        let mut last = last0;
        let mut l = middle - first;
        let mut r = last - middle;

        while l > 0 && r > 0 {
            if l == r {
                self.ss_block_swap(first, middle, l);
                break;
            }

            if l < r {
                let mut a = last - 1;
                let mut b = middle - 1;
                let mut t = self.sa[a as usize];

                loop {
                    self.sa[a as usize] = self.sa[b as usize];
                    a -= 1;
                    self.sa[b as usize] = self.sa[a as usize];
                    b -= 1;

                    if b < first {
                        self.sa[a as usize] = t;
                        last = a;
                        r -= l + 1;

                        if r <= l {
                            break;
                        }

                        a -= 1;
                        b = middle - 1;
                        t = self.sa[a as usize];
                    }
                }
            } else {
                let mut a = first;
                let mut b = middle;
                let mut t = self.sa[a as usize];

                loop {
                    self.sa[a as usize] = self.sa[b as usize];
                    a += 1;
                    self.sa[b as usize] = self.sa[a as usize];
                    b += 1;

                    if last <= b {
                        self.sa[a as usize] = t;
                        first = a + 1;
                        l -= r + 1;

                        if l <= r {
                            break;
                        }

                        a += 1;
                        b = middle;
                        t = self.sa[a as usize];
                    }
                }
            }
        }
    }

    fn ss_block_swap(&mut self, mut a: i32, mut b: i32, mut n: i32) {
        while n > 0 {
            n -= 1;
            self.sa.swap(a as usize, b as usize);
            a += 1;
            b += 1;
        }
    }

    fn ss_swap_merge(&mut self, pa: i32, first0: i32, middle0: i32, last0: i32, buf: i32, buf_size: i32, depth: i32) {
        let mut first = first0;
        let mut middle = middle0;
        let mut last = last0;
        let mut check = 0i32;

        loop {
            if last - middle <= buf_size {
                if first < middle && middle < last {
                    self.ss_merge_backward(pa, first, middle, last, buf, depth);
                }

                if (check & 1) != 0
                    || ((check & 2) != 0
                        && self.ss_compare(pa + get_index(self.sa[(first - 1) as usize]), pa + self.sa[first as usize], depth) == 0)
                {
                    self.sa[first as usize] = !self.sa[first as usize];
                }

                if (check & 4) != 0
                    && self.ss_compare(pa + get_index(self.sa[(last - 1) as usize]), pa + self.sa[last as usize], depth) == 0
                {
                    self.sa[last as usize] = !self.sa[last as usize];
                }

                match self.merge_stack.pop() {
                    None => return,
                    Some(se) => {
                        first = se.a;
                        middle = se.b;
                        last = se.c;
                        check = se.d;
                    }
                }

                continue;
            }

            if middle - first <= buf_size {
                if first < middle {
                    self.ss_merge_forward(pa, first, middle, last, buf, depth);
                }

                if (check & 1) != 0
                    || ((check & 2) != 0
                        && self.ss_compare(pa + get_index(self.sa[(first - 1) as usize]), pa + self.sa[first as usize], depth) == 0)
                {
                    self.sa[first as usize] = !self.sa[first as usize];
                }

                if (check & 4) != 0
                    && self.ss_compare(pa + get_index(self.sa[(last - 1) as usize]), pa + self.sa[last as usize], depth) == 0
                {
                    self.sa[last as usize] = !self.sa[last as usize];
                }

                match self.merge_stack.pop() {
                    None => return,
                    Some(se) => {
                        first = se.a;
                        middle = se.b;
                        last = se.c;
                        check = se.d;
                    }
                }

                continue;
            }

            let mut len = if middle - first < last - middle { middle - first } else { last - middle };
            let mut m = 0i32;
            let mut half = len >> 1;

            while len > 0 {
                let idx1 = pa + get_index(self.sa[(middle + m + half) as usize]);
                let idx2 = pa + get_index(self.sa[(middle - m - half - 1) as usize]);

                if self.ss_compare(idx1, idx2, depth) < 0 {
                    m += half + 1;
                    half -= (len & 1) ^ 1;
                }

                len = half;
                half >>= 1;
            }

            if m > 0 {
                let lm = middle - m;
                let rm = middle + m;
                self.ss_block_swap(lm, middle, m);
                let mut l = middle;
                let mut r = l;
                let mut next = 0i32;

                if rm < last {
                    if self.sa[rm as usize] < 0 {
                        self.sa[rm as usize] = !self.sa[rm as usize];

                        if first < lm {
                            l -= 1;

                            while self.sa[l as usize] < 0 {
                                l -= 1;
                            }

                            next |= 4;
                        }

                        next |= 1;
                    } else if first < lm {
                        while self.sa[r as usize] < 0 {
                            r += 1;
                        }

                        next |= 2;
                    }
                }

                if l - first <= last - r {
                    self.merge_stack.push(r, rm, last, (next & 3) | (check & 4), 0);
                    middle = lm;
                    last = l;
                    check = (check & 3) | (next & 4);
                } else {
                    if r == middle && (next & 2) != 0 {
                        next ^= 6;
                    }

                    self.merge_stack.push(first, lm, l, (check & 3) | (next & 4), 0);
                    first = r;
                    middle = rm;
                    check = (next & 3) | (check & 4);
                }
            } else {
                if self.ss_compare(pa + get_index(self.sa[(middle - 1) as usize]), pa + self.sa[middle as usize], depth) == 0 {
                    self.sa[middle as usize] = !self.sa[middle as usize];
                }

                if (check & 1) != 0
                    || ((check & 2) != 0
                        && self.ss_compare(pa + get_index(self.sa[(first - 1) as usize]), pa + self.sa[first as usize], depth) == 0)
                {
                    self.sa[first as usize] = !self.sa[first as usize];
                }

                if (check & 4) != 0
                    && self.ss_compare(pa + get_index(self.sa[(last - 1) as usize]), pa + self.sa[last as usize], depth) == 0
                {
                    self.sa[last as usize] = !self.sa[last as usize];
                }

                match self.merge_stack.pop() {
                    None => return,
                    Some(se) => {
                        first = se.a;
                        middle = se.b;
                        last = se.c;
                        check = se.d;
                    }
                }
            }
        }
    }

    fn ss_merge_forward(&mut self, pa: i32, first: i32, middle: i32, last: i32, buf: i32, depth: i32) {
        let buf_end = buf + middle - first - 1;
        self.ss_block_swap(buf, first, middle - first);

        let mut a = first;
        let mut b = buf;
        let mut c = middle;
        let t = self.sa[a as usize];

        loop {
            let r = self.ss_compare(pa + self.sa[b as usize], pa + self.sa[c as usize], depth);

            if r < 0 {
                loop {
                    self.sa[a as usize] = self.sa[b as usize];
                    a += 1;

                    if buf_end <= b {
                        self.sa[buf_end as usize] = t;
                        return;
                    }

                    self.sa[b as usize] = self.sa[a as usize];
                    b += 1;

                    if !(self.sa[b as usize] < 0) {
                        break;
                    }
                }
            } else if r > 0 {
                loop {
                    self.sa[a as usize] = self.sa[c as usize];
                    a += 1;
                    self.sa[c as usize] = self.sa[a as usize];
                    c += 1;

                    if last <= c {
                        while b < buf_end {
                            self.sa[a as usize] = self.sa[b as usize];
                            a += 1;
                            self.sa[b as usize] = self.sa[a as usize];
                            b += 1;
                        }

                        self.sa[a as usize] = self.sa[b as usize];
                        self.sa[b as usize] = t;
                        return;
                    }

                    if !(self.sa[c as usize] < 0) {
                        break;
                    }
                }
            } else {
                self.sa[c as usize] = !self.sa[c as usize];

                loop {
                    self.sa[a as usize] = self.sa[b as usize];
                    a += 1;

                    if buf_end <= b {
                        self.sa[buf_end as usize] = t;
                        return;
                    }

                    self.sa[b as usize] = self.sa[a as usize];
                    b += 1;

                    if !(self.sa[b as usize] < 0) {
                        break;
                    }
                }

                loop {
                    self.sa[a as usize] = self.sa[c as usize];
                    a += 1;
                    self.sa[c as usize] = self.sa[a as usize];
                    c += 1;

                    if last <= c {
                        while b < buf_end {
                            self.sa[a as usize] = self.sa[b as usize];
                            a += 1;
                            self.sa[b as usize] = self.sa[a as usize];
                            b += 1;
                        }

                        self.sa[a as usize] = self.sa[b as usize];
                        self.sa[b as usize] = t;
                        return;
                    }

                    if !(self.sa[c as usize] < 0) {
                        break;
                    }
                }
            }
        }
    }

    fn ss_merge_backward(&mut self, pa: i32, first: i32, middle: i32, last: i32, buf: i32, depth: i32) {
        let buf_end = buf + last - middle - 1;
        self.ss_block_swap(buf, middle, last - middle);

        let mut x = 0i32;
        let mut p1;
        let mut p2;

        if self.sa[buf_end as usize] < 0 {
            p1 = pa + !self.sa[buf_end as usize];
            x |= 1;
        } else {
            p1 = pa + self.sa[buf_end as usize];
        }

        if self.sa[(middle - 1) as usize] < 0 {
            p2 = pa + !self.sa[(middle - 1) as usize];
            x |= 2;
        } else {
            p2 = pa + self.sa[(middle - 1) as usize];
        }

        let mut a = last - 1;
        let mut b = buf_end;
        let mut c = middle - 1;
        let t = self.sa[a as usize];

        loop {
            let r = self.ss_compare(p1, p2, depth);

            if r > 0 {
                if (x & 1) != 0 {
                    loop {
                        self.sa[a as usize] = self.sa[b as usize];
                        a -= 1;
                        self.sa[b as usize] = self.sa[a as usize];
                        b -= 1;

                        if !(self.sa[b as usize] < 0) {
                            break;
                        }
                    }

                    x ^= 1;
                }

                self.sa[a as usize] = self.sa[b as usize];
                a -= 1;

                if b <= buf {
                    self.sa[buf as usize] = t;
                    break;
                }

                self.sa[b as usize] = self.sa[a as usize];
                b -= 1;

                if self.sa[b as usize] < 0 {
                    p1 = pa + !self.sa[b as usize];
                    x |= 1;
                } else {
                    p1 = pa + self.sa[b as usize];
                }
            } else if r < 0 {
                if (x & 2) != 0 {
                    loop {
                        self.sa[a as usize] = self.sa[c as usize];
                        a -= 1;
                        self.sa[c as usize] = self.sa[a as usize];
                        c -= 1;

                        if !(self.sa[c as usize] < 0) {
                            break;
                        }
                    }

                    x ^= 2;
                }

                self.sa[a as usize] = self.sa[c as usize];
                a -= 1;
                self.sa[c as usize] = self.sa[a as usize];
                c -= 1;

                if c < first {
                    while buf < b {
                        self.sa[a as usize] = self.sa[b as usize];
                        a -= 1;
                        self.sa[b as usize] = self.sa[a as usize];
                        b -= 1;
                    }

                    self.sa[a as usize] = self.sa[b as usize];
                    self.sa[b as usize] = t;
                    break;
                }

                if self.sa[c as usize] < 0 {
                    p2 = pa + !self.sa[c as usize];
                    x |= 2;
                } else {
                    p2 = pa + self.sa[c as usize];
                }
            } else {
                // r == 0
                if (x & 1) != 0 {
                    loop {
                        self.sa[a as usize] = self.sa[b as usize];
                        a -= 1;
                        self.sa[b as usize] = self.sa[a as usize];
                        b -= 1;

                        if !(self.sa[b as usize] < 0) {
                            break;
                        }
                    }

                    x ^= 1;
                }

                self.sa[a as usize] = !self.sa[b as usize];
                a -= 1;

                if b <= buf {
                    self.sa[buf as usize] = t;
                    break;
                }

                self.sa[b as usize] = self.sa[a as usize];
                b -= 1;

                if (x & 2) != 0 {
                    loop {
                        self.sa[a as usize] = self.sa[c as usize];
                        a -= 1;
                        self.sa[c as usize] = self.sa[a as usize];
                        c -= 1;

                        if !(self.sa[c as usize] < 0) {
                            break;
                        }
                    }

                    x ^= 2;
                }

                self.sa[a as usize] = self.sa[c as usize];
                a -= 1;
                self.sa[c as usize] = self.sa[a as usize];
                c -= 1;

                if c < first {
                    while buf < b {
                        self.sa[a as usize] = self.sa[b as usize];
                        a -= 1;
                        self.sa[b as usize] = self.sa[a as usize];
                        b -= 1;
                    }

                    self.sa[a as usize] = self.sa[b as usize];
                    self.sa[b as usize] = t;
                    break;
                }

                if self.sa[b as usize] < 0 {
                    p1 = pa + !self.sa[b as usize];
                    x |= 1;
                } else {
                    p1 = pa + self.sa[b as usize];
                }

                if self.sa[c as usize] < 0 {
                    p2 = pa + !self.sa[c as usize];
                    x |= 2;
                } else {
                    p2 = pa + self.sa[c as usize];
                }
            }
        }
    }

    fn ss_insertion_sort(&mut self, pa: i32, first: i32, last: i32, depth: i32) {
        for i in (first..=(last - 2)).rev() {
            let t = pa + self.sa[i as usize];
            let mut j = i + 1;
            let mut r;

            loop {
                r = self.ss_compare(t, pa + self.sa[j as usize], depth);

                if !(r > 0) {
                    break;
                }

                loop {
                    self.sa[(j - 1) as usize] = self.sa[j as usize];
                    j += 1;

                    if !(j < last && self.sa[j as usize] < 0) {
                        break;
                    }
                }

                if j >= last {
                    break;
                }
            }

            self.sa[j as usize] = if r == 0 { !self.sa[j as usize] } else { self.sa[j as usize] };
            self.sa[(j - 1) as usize] = t - pa;
        }
    }

    fn ss_multi_key_intro_sort(&mut self, pa: i32, first0: i32, last0: i32, depth0: i32) {
        let mut first = first0;
        let mut last = last0;
        let mut depth = depth0;
        let mut limit = ss_ilg(last - first);
        let mut x = 0i32;

        loop {
            if last - first <= SS_INSERTIONSORT_THRESHOLD {
                if last - first > 1 {
                    self.ss_insertion_sort(pa, first, last, depth);
                }

                match self.ss_stack.pop() {
                    None => return,
                    Some(se) => {
                        first = se.a;
                        last = se.b;
                        depth = se.c;
                        limit = se.d;
                    }
                }

                continue;
            }

            let idx = depth;

            if limit == 0 {
                self.ss_heap_sort(idx, pa, first, last - first);
            }

            limit -= 1;
            let mut a;

            if limit < 0 {
                let mut v = self.ss_char(idx, pa, first);

                a = first + 1;
                while a < last {
                    x = self.ss_char(idx, pa, a);

                    if x != v {
                        if a - first > 1 {
                            break;
                        }

                        v = x;
                        first = a;
                    }

                    a += 1;
                }

                if (self.buffer[(idx + self.sa[(pa + self.sa[first as usize]) as usize] - 1) as usize] as i32) < v {
                    first = self.ss_partition(pa, first, a, depth);
                }

                if a - first <= last - a {
                    if a - first > 1 {
                        self.ss_stack.push(a, last, depth, -1, 0);
                        last = a;
                        depth += 1;
                        limit = ss_ilg(a - first);
                    } else {
                        first = a;
                        limit = -1;
                    }
                } else if last - a > 1 {
                    self.ss_stack.push(first, a, depth + 1, ss_ilg(a - first), 0);
                    first = a;
                    limit = -1;
                } else {
                    last = a;
                    depth += 1;
                    limit = ss_ilg(a - first);
                }

                continue;
            }

            // choose pivot
            a = self.ss_pivot(idx, pa, first, last);
            let v = self.ss_char(idx, pa, a);
            self.sa.swap(first as usize, a as usize);
            let mut b = first;

            // partition
            loop {
                b += 1;
                if !(b < last) {
                    break;
                }
                x = self.ss_char(idx, pa, b);
                if x != v {
                    break;
                }
            }

            a = b;

            if a < last && x < v {
                loop {
                    b += 1;
                    if !(b < last) {
                        break;
                    }
                    x = self.ss_char(idx, pa, b);
                    if x > v {
                        break;
                    }

                    if x == v {
                        self.sa.swap(b as usize, a as usize);
                        a += 1;
                    }
                }
            }

            let mut c = last;

            loop {
                c -= 1;
                if !(c > b) {
                    break;
                }
                x = self.ss_char(idx, pa, c);
                if x != v {
                    break;
                }
            }

            let mut d = c;

            if b < d && x > v {
                loop {
                    c -= 1;
                    if !(c > b) {
                        break;
                    }
                    x = self.ss_char(idx, pa, c);
                    if x < v {
                        break;
                    }

                    if x == v {
                        self.sa.swap(c as usize, d as usize);
                        d -= 1;
                    }
                }
            }

            while b < c {
                self.sa.swap(b as usize, c as usize);

                loop {
                    b += 1;
                    if !(b < c) {
                        break;
                    }
                    x = self.ss_char(idx, pa, b);
                    if x > v {
                        break;
                    }

                    if x == v {
                        self.sa.swap(b as usize, a as usize);
                        a += 1;
                    }
                }

                loop {
                    c -= 1;
                    if !(c > b) {
                        break;
                    }
                    x = self.ss_char(idx, pa, c);
                    if x < v {
                        break;
                    }

                    if x == v {
                        self.sa.swap(c as usize, d as usize);
                        d -= 1;
                    }
                }
            }

            if a <= d {
                c = b - 1;
                let mut s = if a - first > b - a { b - a } else { a - first };

                {
                    let mut e = first;
                    let mut f = b - s;
                    while s > 0 {
                        self.sa.swap(e as usize, f as usize);
                        s -= 1;
                        e += 1;
                        f += 1;
                    }
                }

                s = if d - c > last - d - 1 { last - d - 1 } else { d - c };

                {
                    let mut e = b;
                    let mut f = last - s;
                    while s > 0 {
                        self.sa.swap(e as usize, f as usize);
                        s -= 1;
                        e += 1;
                        f += 1;
                    }
                }

                a = first + (b - a);
                c = last - (d - c);
                b = if v <= self.buffer[(idx + self.sa[(pa + self.sa[a as usize]) as usize] - 1) as usize] as i32 {
                    a
                } else {
                    self.ss_partition(pa, a, c, depth)
                };

                if a - first <= last - c {
                    if last - c <= c - b {
                        self.ss_stack.push(b, c, depth + 1, ss_ilg(c - b), 0);
                        self.ss_stack.push(c, last, depth, limit, 0);
                        last = a;
                    } else if a - first <= c - b {
                        self.ss_stack.push(c, last, depth, limit, 0);
                        self.ss_stack.push(b, c, depth + 1, ss_ilg(c - b), 0);
                        last = a;
                    } else {
                        self.ss_stack.push(c, last, depth, limit, 0);
                        self.ss_stack.push(first, a, depth, limit, 0);
                        first = b;
                        last = c;
                        depth += 1;
                        limit = ss_ilg(c - b);
                    }
                } else if a - first <= c - b {
                    self.ss_stack.push(b, c, depth + 1, ss_ilg(c - b), 0);
                    self.ss_stack.push(first, a, depth, limit, 0);
                    first = c;
                } else if last - c <= c - b {
                    self.ss_stack.push(first, a, depth, limit, 0);
                    self.ss_stack.push(b, c, depth + 1, ss_ilg(c - b), 0);
                    first = c;
                } else {
                    self.ss_stack.push(first, a, depth, limit, 0);
                    self.ss_stack.push(c, last, depth, limit, 0);
                    first = b;
                    last = c;
                    depth += 1;
                    limit = ss_ilg(c - b);
                }
            } else if (self.buffer[(idx + self.sa[(pa + self.sa[first as usize]) as usize] - 1) as usize] as i32) < v {
                first = self.ss_partition(pa, first, last, depth);
                limit = ss_ilg(last - first);
                depth += 1;
            } else {
                limit += 1;
                depth += 1;
            }
        }
    }

    fn ss_pivot(&self, td: i32, pa: i32, first0: i32, last0: i32) -> i32 {
        let mut first = first0;
        let mut last = last0;
        let mut t = last - first;
        let mut middle = first + (t >> 1);

        if t <= 512 {
            return if t <= 32 {
                self.ss_median3(td, pa, first, middle, last - 1)
            } else {
                self.ss_median5(td, pa, first, first + (t >> 2), middle, last - 1 - (t >> 2), last - 1)
            };
        }

        t >>= 3;
        first = self.ss_median3(td, pa, first, first + t, first + (t << 1));
        middle = self.ss_median3(td, pa, middle - t, middle, middle + t);
        last = self.ss_median3(td, pa, last - 1 - (t << 1), last - 1 - t, last - 1);
        self.ss_median3(td, pa, first, middle, last)
    }

    fn ss_median5(&self, td: i32, pa: i32, v1_0: i32, v2_0: i32, v3_0: i32, v4_0: i32, v5_0: i32) -> i32 {
        let mut v1 = v1_0;
        let mut v2 = v2_0;
        let mut v3 = v3_0;
        let mut v4 = v4_0;
        let mut v5 = v5_0;

        if self.ss_char(td, pa, v2) > self.ss_char(td, pa, v3) {
            std::mem::swap(&mut v2, &mut v3);
        }

        if self.ss_char(td, pa, v4) > self.ss_char(td, pa, v5) {
            std::mem::swap(&mut v4, &mut v5);
        }

        if self.ss_char(td, pa, v2) > self.ss_char(td, pa, v4) {
            v4 = v2;
            std::mem::swap(&mut v3, &mut v5);
        }

        if self.ss_char(td, pa, v1) > self.ss_char(td, pa, v3) {
            std::mem::swap(&mut v1, &mut v3);
        }

        if self.ss_char(td, pa, v1) > self.ss_char(td, pa, v4) {
            v4 = v1;
            v3 = v5;
        }

        if self.ss_char(td, pa, v3) > self.ss_char(td, pa, v4) { v4 } else { v3 }
    }

    fn ss_median3(&self, td: i32, pa: i32, v1_0: i32, v2_0: i32, v3: i32) -> i32 {
        let mut v1 = v1_0;
        let mut v2 = v2_0;

        if self.ss_char(td, pa, v1) > self.ss_char(td, pa, v2) {
            std::mem::swap(&mut v1, &mut v2);
        }

        if self.ss_char(td, pa, v2) > self.ss_char(td, pa, v3) {
            return if self.ss_char(td, pa, v1) > self.ss_char(td, pa, v3) { v1 } else { v3 };
        }

        v2
    }

    fn ss_partition(&mut self, pa: i32, first: i32, last: i32, depth: i32) -> i32 {
        let mut a = first - 1;
        let mut b = last;
        let d = depth - 1;
        let pb = pa + 1;

        loop {
            a += 1;

            while a < b && self.sa[(pa + self.sa[a as usize]) as usize] + d >= self.sa[(pb + self.sa[a as usize]) as usize] {
                self.sa[a as usize] = !self.sa[a as usize];
                a += 1;
            }

            b -= 1;

            while b > a && self.sa[(pa + self.sa[b as usize]) as usize] + d < self.sa[(pb + self.sa[b as usize]) as usize] {
                b -= 1;
            }

            if b <= a {
                break;
            }

            let t = !self.sa[b as usize];
            self.sa[b as usize] = self.sa[a as usize];
            self.sa[a as usize] = t;
        }

        if first < a {
            self.sa[first as usize] = !self.sa[first as usize];
        }

        a
    }

    fn ss_heap_sort(&mut self, idx: i32, pa: i32, sa_idx: i32, size: i32) {
        let mut m = size;

        if (size & 1) == 0 {
            m -= 1;

            if self.ss_char(idx, pa, sa_idx + (m >> 1)) < self.ss_char(idx, pa, sa_idx + m) {
                self.sa.swap((sa_idx + m) as usize, (sa_idx + (m >> 1)) as usize);
            }
        }

        for i in (0..=((m >> 1) - 1)).rev() {
            self.ss_fix_down(idx, pa, sa_idx, i, m);
        }

        if (size & 1) == 0 {
            self.sa.swap(sa_idx as usize, (sa_idx + m) as usize);
            self.ss_fix_down(idx, pa, sa_idx, 0, m);
        }

        for i in (1..=(m - 1)).rev() {
            let t = self.sa[sa_idx as usize];
            self.sa[sa_idx as usize] = self.sa[(sa_idx + i) as usize];
            self.ss_fix_down(idx, pa, sa_idx, 0, i);
            self.sa[(sa_idx + i) as usize] = t;
        }
    }

    fn ss_fix_down(&mut self, idx: i32, pa: i32, sa_idx: i32, i0: i32, size: i32) {
        let mut i = i0;
        let v = self.sa[(sa_idx + i) as usize];
        let c = self.ss_char_val(idx, pa, v);
        let mut j = (i << 1) + 1;

        while j < size {
            let mut k = j;
            j += 1;
            let mut d = self.ss_char(idx, pa, sa_idx + k);
            let e = self.ss_char(idx, pa, sa_idx + j);

            if d < e {
                k = j;
                d = e;
            }

            if d <= c {
                break;
            }

            self.sa[(sa_idx + i) as usize] = self.sa[(sa_idx + k) as usize];
            i = k;
            j = (i << 1) + 1;
        }

        self.sa[(i + sa_idx) as usize] = v;
    }

    // -------------------------------------------------------------
    // trSort and the tr* (Larsson-Sadakane) family
    // -------------------------------------------------------------

    fn tr_sort(&mut self, n: i32, depth: i32) {
        let mut budget = TrBudget::new(tr_ilg(n) * 2 / 3, n);
        let mut isad = n + depth;

        while self.sa[0] > -n {
            let mut first = 0i32;
            let mut skip = 0i32;
            let mut unsorted = 0i32;

            loop {
                let t = self.sa[first as usize];

                if t < 0 {
                    first -= t;
                    skip += t;

                    if first < n {
                        continue;
                    } else {
                        break;
                    }
                }

                if skip != 0 {
                    self.sa[(first + skip) as usize] = skip;
                    skip = 0;
                }

                let last = self.sa[(n + t) as usize] + 1;

                if last - first > 1 {
                    budget.count = 0;
                    self.tr_intro_sort(n, isad, first, last, &mut budget);

                    if budget.count != 0 {
                        unsorted += budget.count;
                    } else {
                        skip = first - last;
                    }
                } else if last - first == 1 {
                    skip = -1;
                }

                first = last;

                if !(first < n) {
                    break;
                }
            }

            if skip != 0 {
                self.sa[(first + skip) as usize] = skip;
            }

            if unsorted == 0 {
                break;
            }

            isad += isad - n;
        }
    }

    fn tr_partition(&mut self, isad: i32, first0: i32, middle: i32, last0: i32, v: i32) -> (i32, i32) {
        let mut first = first0;
        let mut last = last0;
        let mut x = 0i32;
        let mut b = middle;

        while b < last {
            x = self.tr_char(isad, b);
            if x != v {
                break;
            }
            b += 1;
        }

        let mut a = b;

        if a < last && x < v {
            loop {
                b += 1;
                if !(b < last) {
                    break;
                }
                x = self.tr_char(isad, b);
                if x > v {
                    break;
                }
                if x == v {
                    self.sa.swap(a as usize, b as usize);
                    a += 1;
                }
            }
        }

        let mut c = last - 1;

        while c > b {
            x = self.tr_char(isad, c);
            if x != v {
                break;
            }
            c -= 1;
        }

        let mut d = c;

        if b < d && x > v {
            loop {
                c -= 1;
                if !(c > b) {
                    break;
                }
                x = self.tr_char(isad, c);
                if x < v {
                    break;
                }
                if x == v {
                    self.sa.swap(c as usize, d as usize);
                    d -= 1;
                }
            }
        }

        while b < c {
            self.sa.swap(c as usize, b as usize);

            loop {
                b += 1;
                if !(b < c) {
                    break;
                }
                x = self.tr_char(isad, b);
                if !(x <= v) {
                    break;
                }

                if x == v {
                    self.sa.swap(a as usize, b as usize);
                    a += 1;
                }
            }

            loop {
                c -= 1;
                if !(c > b) {
                    break;
                }
                x = self.tr_char(isad, c);
                if !(x >= v) {
                    break;
                }

                if x == v {
                    self.sa.swap(c as usize, d as usize);
                    d -= 1;
                }
            }
        }

        if a <= d {
            c = b - 1;
            let mut s = a - first;
            if s > b - a {
                s = b - a;
            }

            {
                let mut e = first;
                let mut f = b - s;
                while s > 0 {
                    self.sa.swap(e as usize, f as usize);
                    s -= 1;
                    e += 1;
                    f += 1;
                }
            }

            s = d - c;
            if s >= last - d {
                s = last - d - 1;
            }

            {
                let mut e = b;
                let mut f = last - s;
                while s > 0 {
                    self.sa.swap(e as usize, f as usize);
                    s -= 1;
                    e += 1;
                    f += 1;
                }
            }

            first += b - a;
            last -= d - c;
        }

        (first, last)
    }

    fn tr_intro_sort(&mut self, isa: i32, isad0: i32, first0: i32, last0: i32, budget: &mut TrBudget) {
        let mut isad = isad0;
        let mut first = first0;
        let mut last = last0;
        let incr = isad - isa;
        let mut limit = tr_ilg(last - first);
        let mut trlink = -1i32;

        loop {
            if limit < 0 {
                if limit == -1 {
                    // tandem repeat partition
                    let (a, b) = self.tr_partition(isad - incr, first, first, last, last - 1);

                    if a < last {
                        let v = a - 1;
                        for c in first..a {
                            let s = self.sa[c as usize];
                            self.sa[(isa + s) as usize] = v;
                        }
                    }

                    if b < last {
                        let v = b - 1;
                        for c in a..b {
                            let s = self.sa[c as usize];
                            self.sa[(isa + s) as usize] = v;
                        }
                    }

                    if b - a > 1 {
                        self.tr_stack.push(0, a, b, 0, 0);
                        self.tr_stack.push(isad - incr, first, last, -2, trlink);
                        trlink = self.tr_stack.size() - 2;
                    }

                    if a - first <= last - b {
                        if a - first > 1 {
                            self.tr_stack.push(isad, b, last, tr_ilg(last - b), trlink);
                            last = a;
                            limit = tr_ilg(a - first);
                        } else if last - b > 1 {
                            first = b;
                            limit = tr_ilg(last - b);
                        } else {
                            match self.tr_stack.pop() {
                                None => return,
                                Some(se) => {
                                    isad = se.a;
                                    first = se.b;
                                    last = se.c;
                                    limit = se.d;
                                    trlink = se.e;
                                }
                            }
                        }
                    } else if last - b > 1 {
                        self.tr_stack.push(isad, first, a, tr_ilg(a - first), trlink);
                        first = b;
                        limit = tr_ilg(last - b);
                    } else if a - first > 1 {
                        last = a;
                        limit = tr_ilg(a - first);
                    } else {
                        match self.tr_stack.pop() {
                            None => return,
                            Some(se) => {
                                isad = se.a;
                                first = se.b;
                                last = se.c;
                                limit = se.d;
                                trlink = se.e;
                            }
                        }
                    }
                } else if limit == -2 {
                    // tandem repeat copy
                    let se = match self.tr_stack.pop() {
                        None => return,
                        Some(se) => se,
                    };

                    if se.d == 0 {
                        self.tr_copy(isa, first, se.b, se.c, last, isad - isa);
                    } else {
                        if trlink >= 0 {
                            self.tr_stack.set_d(trlink, -1);
                        }

                        self.tr_partial_copy(isa, first, se.b, se.c, last, isad - isa);
                    }

                    match self.tr_stack.pop() {
                        None => return,
                        Some(se2) => {
                            isad = se2.a;
                            first = se2.b;
                            last = se2.c;
                            limit = se2.d;
                            trlink = se2.e;
                        }
                    }
                } else {
                    // sorted partition
                    if self.sa[first as usize] >= 0 {
                        let mut a = first;

                        loop {
                            self.sa[(isa + self.sa[a as usize]) as usize] = a;
                            a += 1;

                            if !(a < last && self.sa[a as usize] >= 0) {
                                break;
                            }
                        }

                        first = a;
                    }

                    if first < last {
                        let mut a = first;

                        loop {
                            self.sa[a as usize] = !self.sa[a as usize];
                            a += 1;

                            if !(self.sa[a as usize] < 0) {
                                break;
                            }
                        }

                        let next = if self.sa[(isa + self.sa[a as usize]) as usize]
                            != self.sa[(isad + self.sa[a as usize]) as usize]
                        {
                            tr_ilg(a - first + 1)
                        } else {
                            -1
                        };

                        a += 1;

                        if a < last {
                            let v = a - 1;

                            for b in first..a {
                                self.sa[(isa + self.sa[b as usize]) as usize] = v;
                            }
                        }

                        if budget.check(a - first) {
                            if a - first <= last - a {
                                self.tr_stack.push(isad, a, last, -3, trlink);
                                isad += incr;
                                last = a;
                                limit = next;
                            } else if last - a > 1 {
                                self.tr_stack.push(isad + incr, first, a, next, trlink);
                                first = a;
                                limit = -3;
                            } else {
                                isad += incr;
                                last = a;
                                limit = next;
                            }
                        } else {
                            if trlink >= 0 {
                                self.tr_stack.set_d(trlink, -1);
                            }

                            if last - a > 1 {
                                first = a;
                                limit = -3;
                            } else {
                                match self.tr_stack.pop() {
                                    None => return,
                                    Some(se) => {
                                        isad = se.a;
                                        first = se.b;
                                        last = se.c;
                                        limit = se.d;
                                        trlink = se.e;
                                    }
                                }
                            }
                        }
                    } else {
                        match self.tr_stack.pop() {
                            None => return,
                            Some(se) => {
                                isad = se.a;
                                first = se.b;
                                last = se.c;
                                limit = se.d;
                                trlink = se.e;
                            }
                        }
                    }
                }

                continue;
            }

            if last - first <= TR_INSERTIONSORT_THRESHOLD {
                self.tr_insertion_sort(isad, first, last);
                limit = -3;
                continue;
            }

            if limit == 0 {
                self.tr_heap_sort(isad, first, last - first);
                let mut a = last - 1;

                while first < a {
                    let mut b = a - 1;
                    let x = self.sa[(isad + self.sa[a as usize]) as usize];

                    while first <= b && self.sa[(isad + self.sa[b as usize]) as usize] == x {
                        self.sa[b as usize] = !self.sa[b as usize];
                        b -= 1;
                    }

                    a = b;
                }

                limit = -3;
                continue;
            }

            limit -= 1;

            // choose pivot
            let piv = self.tr_pivot(isad, first, last);
            self.sa.swap(first as usize, piv as usize);
            let mut v = self.sa[(isad + self.sa[first as usize]) as usize];

            // partition
            let (a, b) = self.tr_partition(isad, first, first + 1, last, v);

            if last - first != b - a {
                let next = if self.sa[(isa + self.sa[a as usize]) as usize] != v { tr_ilg(b - a) } else { -1 };
                v = a - 1;

                for c in first..a {
                    self.sa[(isa + self.sa[c as usize]) as usize] = v;
                }

                if b < last {
                    v = b - 1;

                    for c in a..b {
                        self.sa[(isa + self.sa[c as usize]) as usize] = v;
                    }
                }

                if b - a > 1 && budget.check(b - a) {
                    if a - first <= last - b {
                        if last - b <= b - a {
                            if a - first > 1 {
                                self.tr_stack.push(isad + incr, a, b, next, trlink);
                                self.tr_stack.push(isad, b, last, limit, trlink);
                                last = a;
                            } else if last - b > 1 {
                                self.tr_stack.push(isad + incr, a, b, next, trlink);
                                first = b;
                            } else {
                                isad += incr;
                                first = a;
                                last = b;
                                limit = next;
                            }
                        } else if a - first <= b - a {
                            if a - first > 1 {
                                self.tr_stack.push(isad, b, last, limit, trlink);
                                self.tr_stack.push(isad + incr, a, b, next, trlink);
                                last = a;
                            } else {
                                self.tr_stack.push(isad, b, last, limit, trlink);
                                isad += incr;
                                first = a;
                                last = b;
                                limit = next;
                            }
                        } else {
                            self.tr_stack.push(isad, b, last, limit, trlink);
                            self.tr_stack.push(isad, first, a, limit, trlink);
                            isad += incr;
                            first = a;
                            last = b;
                            limit = next;
                        }
                    } else if a - first <= b - a {
                        if last - b > 1 {
                            self.tr_stack.push(isad + incr, a, b, next, trlink);
                            self.tr_stack.push(isad, first, a, limit, trlink);
                            first = b;
                        } else if a - first > 1 {
                            self.tr_stack.push(isad + incr, a, b, next, trlink);
                            last = a;
                        } else {
                            isad += incr;
                            first = a;
                            last = b;
                            limit = next;
                        }
                    } else if last - b <= b - a {
                        if last - b > 1 {
                            self.tr_stack.push(isad, first, a, limit, trlink);
                            self.tr_stack.push(isad + incr, a, b, next, trlink);
                            first = b;
                        } else {
                            self.tr_stack.push(isad, first, a, limit, trlink);
                            isad += incr;
                            first = a;
                            last = b;
                            limit = next;
                        }
                    } else {
                        self.tr_stack.push(isad, first, a, limit, trlink);
                        self.tr_stack.push(isad, b, last, limit, trlink);
                        isad += incr;
                        first = a;
                        last = b;
                        limit = next;
                    }
                } else {
                    if b - a > 1 && trlink >= 0 {
                        self.tr_stack.set_d(trlink, -1);
                    }

                    if a - first <= last - b {
                        if a - first > 1 {
                            self.tr_stack.push(isad, b, last, limit, trlink);
                            last = a;
                        } else if last - b > 1 {
                            first = b;
                        } else {
                            match self.tr_stack.pop() {
                                None => return,
                                Some(se) => {
                                    isad = se.a;
                                    first = se.b;
                                    last = se.c;
                                    limit = se.d;
                                    trlink = se.e;
                                }
                            }
                        }
                    } else if last - b > 1 {
                        self.tr_stack.push(isad, first, a, limit, trlink);
                        first = b;
                    } else if a - first > 1 {
                        last = a;
                    } else {
                        match self.tr_stack.pop() {
                            None => return,
                            Some(se) => {
                                isad = se.a;
                                first = se.b;
                                last = se.c;
                                limit = se.d;
                                trlink = se.e;
                            }
                        }
                    }
                }
            } else if budget.check(last - first) {
                limit = tr_ilg(last - first);
                isad += incr;
            } else {
                if trlink >= 0 {
                    self.tr_stack.set_d(trlink, -1);
                }

                match self.tr_stack.pop() {
                    None => return,
                    Some(se) => {
                        isad = se.a;
                        first = se.b;
                        last = se.c;
                        limit = se.d;
                        trlink = se.e;
                    }
                }
            }
        }
    }

    fn tr_pivot(&self, isad: i32, first0: i32, last0: i32) -> i32 {
        let mut first = first0;
        let mut last = last0;
        let mut t = last - first;
        let mut middle = first + (t >> 1);

        if t <= 512 {
            if t <= 32 {
                return self.tr_median3(isad, first, middle, last - 1);
            }

            t >>= 2;
            return self.tr_median5(isad, first, first + t, middle, last - 1 - t, last - 1);
        }

        t >>= 3;
        first = self.tr_median3(isad, first, first + t, first + (t << 1));
        middle = self.tr_median3(isad, middle - t, middle, middle + t);
        last = self.tr_median3(isad, last - 1 - (t << 1), last - 1 - t, last - 1);
        self.tr_median3(isad, first, middle, last)
    }

    fn tr_median5(&self, isad: i32, v1_0: i32, v2_0: i32, v3_0: i32, v4_0: i32, v5_0: i32) -> i32 {
        let mut v1 = v1_0;
        let mut v2 = v2_0;
        let mut v3 = v3_0;
        let mut v4 = v4_0;
        let mut v5 = v5_0;

        if self.tr_char(isad, v2) > self.tr_char(isad, v3) {
            std::mem::swap(&mut v2, &mut v3);
        }
        if self.tr_char(isad, v4) > self.tr_char(isad, v5) {
            std::mem::swap(&mut v4, &mut v5);
        }
        if self.tr_char(isad, v2) > self.tr_char(isad, v4) {
            std::mem::swap(&mut v2, &mut v4);
            std::mem::swap(&mut v3, &mut v5);
        }
        if self.tr_char(isad, v1) > self.tr_char(isad, v3) {
            std::mem::swap(&mut v1, &mut v3);
        }
        if self.tr_char(isad, v1) > self.tr_char(isad, v4) {
            std::mem::swap(&mut v1, &mut v4);
            std::mem::swap(&mut v3, &mut v5);
        }

        if self.tr_char(isad, v3) > self.tr_char(isad, v4) { v4 } else { v3 }
    }

    fn tr_median3(&self, isad: i32, v1_0: i32, v2_0: i32, v3: i32) -> i32 {
        let mut v1 = v1_0;
        let mut v2 = v2_0;

        if self.tr_char(isad, v1) > self.tr_char(isad, v2) {
            std::mem::swap(&mut v1, &mut v2);
        }

        if self.tr_char(isad, v2) > self.tr_char(isad, v3) {
            return if self.tr_char(isad, v1) > self.tr_char(isad, v3) { v1 } else { v3 };
        }

        v2
    }

    fn tr_heap_sort(&mut self, isad: i32, sa_idx: i32, size: i32) {
        let mut m = size;

        if (size & 1) == 0 {
            m -= 1;

            if self.tr_char(isad, sa_idx + (m >> 1)) < self.tr_char(isad, sa_idx + m) {
                self.sa.swap((sa_idx + m) as usize, (sa_idx + (m >> 1)) as usize);
            }
        }

        for i in (0..=((m >> 1) - 1)).rev() {
            self.tr_fix_down(isad, sa_idx, i, m);
        }

        if (size & 1) == 0 {
            self.sa.swap(sa_idx as usize, (sa_idx + m) as usize);
            self.tr_fix_down(isad, sa_idx, 0, m);
        }

        for i in (1..=(m - 1)).rev() {
            let t = self.sa[sa_idx as usize];
            self.sa[sa_idx as usize] = self.sa[(sa_idx + i) as usize];
            self.tr_fix_down(isad, sa_idx, 0, i);
            self.sa[(sa_idx + i) as usize] = t;
        }
    }

    fn tr_fix_down(&mut self, isad: i32, sa_idx: i32, i0: i32, size: i32) {
        let mut i = i0;
        let v = self.sa[(sa_idx + i) as usize];
        let c = self.tr_char_val(isad, v);
        let mut j = (i << 1) + 1;

        while j < size {
            let mut k = j;
            j += 1;
            let mut d = self.tr_char(isad, sa_idx + k);
            let e = self.tr_char(isad, sa_idx + j);

            if d < e {
                k = j;
                d = e;
            }

            if d <= c {
                break;
            }

            self.sa[(sa_idx + i) as usize] = self.sa[(sa_idx + k) as usize];
            i = k;
            j = (i << 1) + 1;
        }

        self.sa[(sa_idx + i) as usize] = v;
    }

    fn tr_insertion_sort(&mut self, isad: i32, first: i32, last: i32) {
        for a in (first + 1)..last {
            let mut b = a - 1;
            let t = self.sa[a as usize];
            let mut r;

            loop {
                r = self.tr_char_val(isad, t) - self.tr_char(isad, b);

                if !(r < 0) {
                    break;
                }

                loop {
                    self.sa[(b + 1) as usize] = self.sa[b as usize];
                    b -= 1;

                    if !(b >= first && self.sa[b as usize] < 0) {
                        break;
                    }
                }

                if b < first {
                    break;
                }
            }

            if r == 0 {
                self.sa[b as usize] = !self.sa[b as usize];
            }

            self.sa[(b + 1) as usize] = t;
        }
    }

    fn tr_partial_copy(&mut self, isa: i32, first: i32, a: i32, b: i32, last: i32, depth: i32) {
        let v = b - 1;
        let mut last_rank = -1i32;
        let mut new_rank = -1i32;
        let mut d = a - 1;

        let mut c = first;
        while c <= d {
            let s = self.sa[c as usize] - depth;

            if s >= 0 && self.sa[(isa + s) as usize] == v {
                d += 1;
                self.sa[d as usize] = s;
                let rank = self.sa[(isa + s + depth) as usize];

                if last_rank != rank {
                    last_rank = rank;
                    new_rank = d;
                }

                self.sa[(isa + s) as usize] = new_rank;
            }

            c += 1;
        }

        last_rank = -1;

        let mut e = d;
        while first <= e {
            let rank = self.sa[(isa + self.sa[e as usize]) as usize];

            if last_rank != rank {
                last_rank = rank;
                new_rank = e;
            }

            if new_rank != rank {
                self.sa[(isa + self.sa[e as usize]) as usize] = new_rank;
            }

            e -= 1;
        }

        last_rank = -1;
        let e2 = d + 1;
        d = b;

        let mut c2 = last - 1;
        while d > e2 {
            let s = self.sa[c2 as usize] - depth;

            if s >= 0 && self.sa[(isa + s) as usize] == v {
                d -= 1;
                self.sa[d as usize] = s;
                let rank = self.sa[(isa + s + depth) as usize];

                if last_rank != rank {
                    last_rank = rank;
                    new_rank = d;
                }

                self.sa[(isa + s) as usize] = new_rank;
            }

            c2 -= 1;
        }
    }

    fn tr_copy(&mut self, isa: i32, first: i32, a: i32, b: i32, last: i32, depth: i32) {
        let v = b - 1;
        let mut d = a - 1;

        let mut c = first;
        while c <= d {
            let s = self.sa[c as usize] - depth;

            if s >= 0 && self.sa[(isa + s) as usize] == v {
                d += 1;
                self.sa[d as usize] = s;
                self.sa[(isa + s) as usize] = d;
            }

            c += 1;
        }

        let e = d + 1;
        d = b;

        let mut c2 = last - 1;
        while d > e {
            let s = self.sa[c2 as usize] - depth;

            if s >= 0 && self.sa[(isa + s) as usize] == v {
                d -= 1;
                self.sa[d as usize] = s;
                self.sa[(isa + s) as usize] = d;
            }

            c2 -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tiny splitmix64 PRNG so the fuzz tests below need no extra crate
    // dependency and stay perfectly reproducible across runs/platforms.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }

        fn next_usize(&mut self, bound: usize) -> usize {
            (self.next_u64() % (bound as u64)) as usize
        }

        fn random_bytes(&mut self, len: usize, alphabet: u8) -> Vec<u8> {
            (0..len).map(|_| (self.next_u64() % alphabet as u64) as u8).collect()
        }
    }

    // Independent, obviously-correct (if slow) reference: sort all n
    // suffixes with the standard library's comparison sort. Only used on
    // small inputs in these tests, to double-check `sais::suffix_array`
    // itself isn't the one with the bug on any given case.
    fn naive_suffix_array(text: &[u8]) -> Vec<u32> {
        let n = text.len();
        let mut sa: Vec<u32> = (0..n as u32).collect();
        sa.sort_by(|&a, &b| text[a as usize..].cmp(&text[b as usize..]));
        sa
    }

    fn check_matches_sais(text: &[u8]) {
        let expected = crate::sais::suffix_array(text);
        let actual = suffix_array(text);
        assert_eq!(
            actual, expected,
            "divsufsort SA differs from sais SA for input of length {} (first 64 bytes: {:?})",
            text.len(),
            &text[..text.len().min(64)]
        );
    }

    fn check_matches_naive(text: &[u8]) {
        let expected = naive_suffix_array(text);
        let actual = suffix_array(text);
        assert_eq!(actual, expected, "divsufsort SA differs from naive SA for {:?}", text);
    }

    #[test]
    fn empty_and_singleton() {
        assert_eq!(suffix_array(b""), Vec::<u32>::new());
        assert_eq!(suffix_array(b"a"), vec![0u32]);
    }

    #[test]
    fn small_fixed_strings_vs_naive() {
        for s in [
            "aa",
            "ab",
            "ba",
            "aaa",
            "aaaa",
            "banana",
            "mississippi",
            "abracadabra",
            "abcabcabcabc",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            "the quick brown fox jumps over the lazy dog",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab",
        ] {
            check_matches_naive(s.as_bytes());
        }
    }

    #[test]
    fn small_fixed_strings_vs_sais() {
        for s in ["", "a", "aa", "banana", "mississippi", "abracadabra"] {
            check_matches_sais(s.as_bytes());
        }
    }

    #[test]
    fn fuzz_small_random_vs_naive() {
        let mut rng = Rng(0xC0FFEE_u64);

        for _ in 0..2000 {
            let len = rng.next_usize(40);
            let alphabet = [1u8, 2, 4, 26][rng.next_usize(4)];
            let text = rng.random_bytes(len, alphabet);
            check_matches_naive(&text);
        }
    }

    #[test]
    fn fuzz_medium_random_vs_sais() {
        let mut rng = Rng(0xDEADBEEF_u64);

        for _ in 0..300 {
            let len = rng.next_usize(5000);
            let alphabet = [1u8, 2, 4, 26, 255][rng.next_usize(5)];
            let text = rng.random_bytes(len, alphabet);
            check_matches_sais(&text);
        }
    }

    // Sizes chosen to straddle SS_BLOCKSIZE (8192) and to exercise ssSort's
    // buffer-splitting and ssMultiKeyIntroSort's block/stack-recursion
    // paths, which the small fuzz cases above barely touch.
    #[test]
    fn fuzz_block_boundary_sizes_vs_sais() {
        let mut rng = Rng(0x5EED_u64);
        let sizes = [
            1, 2, 3, 100, 8191, 8192, 8193, 16383, 16384, 16385, 20000, 40000, 65536, 100000,
        ];

        for &len in &sizes {
            for &alphabet in &[1u8, 2, 4, 26, 255] {
                let text = rng.random_bytes(len, alphabet);
                check_matches_sais(&text);
            }
        }
    }

    #[test]
    fn fuzz_highly_repetitive_vs_sais() {
        // Long runs and near-periodic text stress the type-B*
        // classification and the tandem-repeat (trSort) path harder than
        // uniform random data does.
        let mut rng = Rng(0xBADC0DE_u64);

        for _ in 0..100 {
            let period = 1 + rng.next_usize(8);
            let pattern = rng.random_bytes(period, 4);
            let reps = 1 + rng.next_usize(3000);
            let mut text = Vec::with_capacity(period * reps);

            for _ in 0..reps {
                text.extend_from_slice(&pattern);
            }

            // A few random single-byte perturbations so it's not perfectly
            // periodic (which would otherwise never terminate a tandem
            // repeat scan meaningfully differently across runs).
            let tweaks = rng.next_usize(5);
            for _ in 0..tweaks {
                if text.is_empty() {
                    break;
                }
                let i = rng.next_usize(text.len());
                text[i] = (rng.next_u64() % 4) as u8;
            }

            check_matches_sais(&text);
        }
    }

    #[test]
    fn real_text_vs_sais() {
        // A real, non-synthetic, moderately large input (this crate's own
        // README), the kind of thing container.rs's own tests already use
        // as a round-trip fixture.
        let text = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
            .expect("README.md should exist");
        check_matches_sais(&text);
    }

    // Multi-MB real/synthetic files (text, DNA-alphabet, base64, a
    // highly-repetitive stream, and a small binary executable) exercising
    // deep ssSort/trSort recursion and large bucket counts that the small
    // fuzz cases above cannot reach. Slow in a debug build (this is an
    // O(n log n)-class algorithm run five times over ~30MB total), so it
    // is `#[ignore]`d by default -- run explicitly with:
    //   cargo test --release --lib divsufsort::tests::large_real_files_vs_sais -- --ignored
    #[test]
    #[ignore]
    fn large_real_files_vs_sais() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/verify");

        for name in ["text_8mb.bin", "dna.bin", "base64_8mb.bin", "repeat_8mb.bin", "kanzi.exe"] {
            let path = format!("{dir}/{name}");
            let text = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
            check_matches_sais(&text);
        }
    }

    // --- computeBWT ---

    // Reference BWT built the same way bwt.rs's Bwt::forward does, but
    // driven by sais::suffix_array instead of this module -- an
    // independent construction to check compute_bwt's output and index
    // bookkeeping against, not just the plain suffix array.
    fn reference_bwt(src: &[u8]) -> (Vec<u8>, usize) {
        let count = src.len();
        let sa = crate::sais::suffix_array(src);
        let mut out = vec![0u8; count];
        out[0] = src[count - 1];
        let mut p_idx = 0usize;
        let mut o = 1usize;

        for (i, &s) in sa.iter().enumerate() {
            if s == 0 {
                p_idx = i;
                continue;
            }

            out[o] = src[s as usize - 1];
            o += 1;
        }

        (out, p_idx)
    }

    fn check_bwt_matches_reference(src: &[u8]) {
        let count = src.len();
        let (expected_bwt, expected_p_idx) = reference_bwt(src);

        let mut output = vec![0u8; count];
        let mut bwt = vec![0i32; count];
        let idx_count = 8i32.min(count as i32).max(1);
        let mut indexes = vec![0i32; idx_count as usize];

        let ok = compute_bwt(src, &mut output, &mut bwt, &mut indexes, idx_count);
        assert!(ok, "compute_bwt failed for input of length {count}");
        assert_eq!(output, expected_bwt, "BWT bytes differ for input of length {count}");
        assert_eq!(indexes[0] as usize, expected_p_idx + 1, "primary index differs for input of length {count}");
    }

    #[test]
    fn bwt_small_fixed_strings() {
        for s in ["ab", "banana", "mississippi", "abracadabra", "aaaaaaaaaa", "aaaaaaaaaab"] {
            check_bwt_matches_reference(s.as_bytes());
        }
    }

    #[test]
    fn bwt_fuzz_vs_reference() {
        let mut rng = Rng(0xB17E5CA1E_u64);

        for _ in 0..300 {
            let len = 2 + rng.next_usize(4000);
            let alphabet = [1u8, 2, 4, 26, 255][rng.next_usize(5)];
            let text = rng.random_bytes(len, alphabet);
            check_bwt_matches_reference(&text);
        }
    }
}
