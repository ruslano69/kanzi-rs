// SA-IS: linear-time (in practice, near-linear here due to the O(n) extra
// allocations per recursion level) suffix array construction via induced
// sorting (Nong, Zhang, Chen, "Linear Suffix Array Construction by Almost
// Pure Induced-Sorting", 2009).
//
// This is NOT a port of kanzi-go's DivSufSort (transform/DivSufSort.go,
// Yuta Mori's algorithm, ~2700 lines of Go-specific bucket/stack
// bookkeeping) -- it's an independent, from-scratch implementation of a
// different (but same complexity class, and in practice comparably fast)
// SA construction algorithm. This is a deliberate choice, not a shortcut:
// bwt.rs's own header comment already establishes that ANY correct suffix
// array construction yields byte-identical BWT output (suffixes of a
// block are pairwise distinct, so the sorted order is unique) -- the
// previous prefix-doubling + radix sort implementation in bwt.rs was
// exactly this same "any correct algorithm will do" reasoning, just O(n
// log n) instead of near-linear. Porting SA-IS from a from-scratch,
// well-documented algorithm description carries far less risk of
// mistranslating DivSufSort's many Go/Java-specific micro-optimizations
// (goroutine-sharded bucket sort, stack-based iterative recursion, etc.)
// while achieving the same asymptotic goal.
//
// Standard "$-sentinel" semantics throughout (a shorter suffix sorts
// first on shared prefixes), matching bwt.rs's existing contract exactly:
// implemented by internally shifting the real alphabet up by 1 and
// appending an explicit 0 sentinel (globally smallest, unique) before
// running the core algorithm, then dropping the sentinel's own position
// (always sorts first) from the result. This sidesteps every "no real
// sentinel" edge case in classification/LMS-finding that a from-scratch
// SA-IS write-up would otherwise need to special-case by hand: a unique
// minimal terminator is exactly what the standard algorithm assumes.

/// Computes the suffix array of `text` (arbitrary bytes) with standard
/// $-sentinel semantics. Returns a Vec of length `text.len()`.
pub fn suffix_array(text: &[u8]) -> Vec<u32> {
    let n = text.len();
    let mut ext: Vec<u32> = Vec::with_capacity(n + 1);

    for &b in text {
        ext.push(b as u32 + 1);
    }

    ext.push(0);

    let sa_ext = sa_is_core(&ext, 257);
    debug_assert_eq!(sa_ext[0] as usize, n, "sentinel must sort first");
    sa_ext[1..].to_vec()
}

/// Same sentinel trick, for the recursive step's already-integer "reduced"
/// alphabet (LMS-substring names, values in [0, k)).
fn suffix_array_reduced(text: &[u32], k: usize) -> Vec<u32> {
    let n = text.len();
    let mut ext: Vec<u32> = Vec::with_capacity(n + 1);

    for &c in text {
        ext.push(c + 1);
    }

    ext.push(0);

    let sa_ext = sa_is_core(&ext, k + 1);
    debug_assert_eq!(sa_ext[0] as usize, n, "sentinel must sort first");
    sa_ext[1..].to_vec()
}

#[inline]
fn is_lms(is_s: &[bool], i: usize) -> bool {
    i > 0 && is_s[i] && !is_s[i - 1]
}

/// Bucket boundaries for `k` symbol values occurring in `text`. `end=true`
/// gives one-past-the-last-slot pointers (for tail-decrement seeding /
/// S-type induction); `end=false` gives first-slot pointers (for
/// head-increment L-type induction).
fn get_buckets(text: &[u32], k: usize, end: bool) -> Vec<u32> {
    let mut count = vec![0u32; k];

    for &c in text {
        count[c as usize] += 1;
    }

    let mut sum = 0u32;
    let mut buckets = vec![0u32; k];

    for i in 0..k {
        sum += count[i];
        buckets[i] = if end { sum } else { sum - count[i] };
    }

    buckets
}

/// Seeds `sa` with `-1` (empty) everywhere, then places each position in
/// `lms` at the tail of its bucket, processed in reverse so that (when
/// `lms` is itself ascending-suffix-sorted) positions land in the correct
/// relative order within a shared bucket. When `lms` is only in left-to-
/// right text order (the first, coarse seeding pass), the specific
/// placement order within a bucket doesn't affect correctness -- SA-IS's
/// induced-sorting theorem guarantees LMS *substrings* come out correctly
/// ordered regardless of how same-bucket LMS entries were seeded.
fn seed_lms(sa: &mut [i32], text: &[u32], k: usize, lms: &[u32]) {
    sa.fill(-1);
    let mut bkt = get_buckets(text, k, true);

    for &p in lms.iter().rev() {
        let c = text[p as usize] as usize;
        bkt[c] -= 1;
        sa[bkt[c] as usize] = p as i32;
    }
}

fn induce_l(sa: &mut [i32], text: &[u32], k: usize, is_s: &[bool]) {
    let mut bkt = get_buckets(text, k, false);

    for i in 0..sa.len() {
        let s = sa[i];

        if s <= 0 {
            continue;
        }

        let j = (s - 1) as usize;

        if !is_s[j] {
            let c = text[j] as usize;
            sa[bkt[c] as usize] = j as i32;
            bkt[c] += 1;
        }
    }
}

fn induce_s(sa: &mut [i32], text: &[u32], k: usize, is_s: &[bool]) {
    let mut bkt = get_buckets(text, k, true);

    for i in (0..sa.len()).rev() {
        let s = sa[i];

        if s <= 0 {
            continue;
        }

        let j = (s - 1) as usize;

        if is_s[j] {
            let c = text[j] as usize;
            bkt[c] -= 1;
            sa[bkt[c] as usize] = j as i32;
        }
    }
}

/// Compares the LMS substrings starting at `p` and `q` (both LMS
/// positions) for exact equality, including their terminating character.
/// `d==0` (the shared starting offset) never counts as a stopping LMS
/// boundary -- an LMS substring starts at an LMS position but extends
/// *to* the next one, so the start itself doesn't end it.
fn lms_substrings_equal(text: &[u32], is_s: &[bool], p: usize, q: usize) -> bool {
    if p == q {
        return true;
    }

    let n = text.len();
    let mut d = 0usize;

    loop {
        let i = p + d;
        let j = q + d;

        if i >= n || j >= n {
            // Guarded fallback; the appended unique sentinel guarantees
            // this is unreachable (see module doc), but never treat it
            // as a match if it somehow were.
            debug_assert!(false, "lms_substrings_equal ran off the end");
            return false;
        }

        let i_lms = d > 0 && is_lms(is_s, i);
        let j_lms = d > 0 && is_lms(is_s, j);

        if d > 0 && (i_lms || j_lms) {
            return i_lms && j_lms && text[i] == text[j];
        }

        if text[i] != text[j] {
            return false;
        }

        d += 1;
    }
}

/// Core SA-IS recursion. `text` must already carry a unique, globally
/// minimal terminating symbol as its last element (see module doc) --
/// callers are `suffix_array`/`suffix_array_reduced`, never end users.
fn sa_is_core(text: &[u32], k: usize) -> Vec<u32> {
    let n = text.len();

    if n == 1 {
        return vec![0];
    }

    // 1. Classify S-type (true) / L-type (false) suffixes, right to left.
    let mut is_s = vec![false; n];
    is_s[n - 1] = true;

    for i in (0..n - 1).rev() {
        is_s[i] = match text[i].cmp(&text[i + 1]) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => is_s[i + 1],
        };
    }

    // 2. LMS positions, in left-to-right text order.
    let lms_positions: Vec<u32> = (1..n).filter(|&i| is_lms(&is_s, i)).map(|i| i as u32).collect();
    debug_assert!(!lms_positions.is_empty(), "sentinel guarantees at least one LMS position");

    // 3. Coarse pass: seed + induce to get LMS *substrings* (not yet full
    // suffixes) correctly ordered relative to each other.
    let mut sa = vec![-1i32; n];
    seed_lms(&mut sa, text, k, &lms_positions);
    induce_l(&mut sa, text, k, &is_s);
    induce_s(&mut sa, text, k, &is_s);

    // 4. Extract LMS positions from `sa` in their now-correct substring
    // order, and name each distinct substring.
    let sorted_lms: Vec<u32> = sa.iter().copied().filter(|&x| x >= 0 && is_lms(&is_s, x as usize)).map(|x| x as u32).collect();
    debug_assert_eq!(sorted_lms.len(), lms_positions.len());

    let mut name_of = vec![u32::MAX; n];
    let mut name = 0u32;
    name_of[sorted_lms[0] as usize] = 0;

    for w in 1..sorted_lms.len() {
        if !lms_substrings_equal(text, &is_s, sorted_lms[w - 1] as usize, sorted_lms[w] as usize) {
            name += 1;
        }

        name_of[sorted_lms[w] as usize] = name;
    }

    let num_names = (name + 1) as usize;

    // 5. Reduced problem: names in original left-to-right LMS order.
    let reduced: Vec<u32> = lms_positions.iter().map(|&p| name_of[p as usize]).collect();

    let sa1: Vec<u32> = if num_names == lms_positions.len() {
        // All LMS substrings distinct: `reduced` is already a permutation
        // of 0..num_names, invert it directly instead of recursing.
        let mut sa1 = vec![0u32; lms_positions.len()];

        for (i, &r) in reduced.iter().enumerate() {
            sa1[r as usize] = i as u32;
        }

        sa1
    } else {
        suffix_array_reduced(&reduced, num_names)
    };

    // 6. Map the recursively-sorted reduced-problem indices back to real
    // LMS positions -- these are now correctly ordered full LMS suffixes.
    let final_lms_order: Vec<u32> = sa1.iter().map(|&i| lms_positions[i as usize]).collect();

    // 7. Final pass: seed with the exact LMS order, induce the rest.
    let mut sa = vec![-1i32; n];
    seed_lms(&mut sa, text, k, &final_lms_order);
    induce_l(&mut sa, text, k, &is_s);
    induce_s(&mut sa, text, k, &is_s);

    sa.into_iter().map(|x| x as u32).collect()
}
