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
//
// Implementation notes (vs. a textbook SA-IS): the S/L classification is
// kept in a bitset rather than a `Vec<bool>` (1 bit/position instead of 1
// byte, so it stays cache-resident for multi-MB blocks); symbol counts are
// computed once per recursion level and reused for every bucket pass
// instead of rescanning the text per pass; the suffix array is `Vec<u32>`
// end to end (no i32->u32 conversion pass) and the sentinel row is dropped
// in place; and the hot induced-sort loops use `get_unchecked` after their
// indices have been proven in range. Together these cut the memory traffic
// of the dominant forward-BWT phase substantially.

#[inline(always)]
fn get_bit(words: &[u64], i: usize) -> bool {
    // i < words.len()*64 by construction at every call site.
    unsafe { (*words.get_unchecked(i >> 6) >> (i & 63)) & 1 != 0 }
}

#[inline(always)]
fn set_bit(words: &mut [u64], i: usize) {
    unsafe {
        let w = words.get_unchecked_mut(i >> 6);
        *w |= 1u64 << (i & 63);
    }
}

#[inline(always)]
fn is_lms(is_s: &[u64], i: usize) -> bool {
    i > 0 && get_bit(is_s, i) && !get_bit(is_s, i - 1)
}

/// Computes the suffix array of `text` (arbitrary bytes) with standard
/// $-sentinel semantics. Returns a Vec of length `text.len()`.
pub fn suffix_array(text: &[u8]) -> Vec<u32> {
    let n = text.len();
    let mut ext: Vec<u32> = Vec::with_capacity(n + 1);

    for &b in text {
        ext.push(b as u32 + 1);
    }

    ext.push(0);

    let mut sa = sa_is_core(&ext, 257);
    debug_assert_eq!(sa[0] as usize, n, "sentinel must sort first");
    // Drop the sentinel row in place instead of allocating a second n-word
    // array just to skip element 0.
    sa.copy_within(1.., 0);
    sa.pop();
    sa
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

    let mut sa = sa_is_core(&ext, k + 1);
    debug_assert_eq!(sa[0] as usize, n, "sentinel must sort first");
    sa.copy_within(1.., 0);
    sa.pop();
    sa
}

/// Symbol counts for `k` symbol values occurring in `text`. One scan of
/// the text feeds both bucket-head and bucket-end construction below.
fn bucket_counts(text: &[u32], k: usize) -> Vec<u32> {
    let mut b = vec![0u32; k];

    for &c in text {
        unsafe {
            *b.get_unchecked_mut(c as usize) += 1;
        }
    }

    b
}

/// Bucket heads (first slot of each bucket) from symbol counts, for
/// head-increment L-type induction.
fn counts_to_heads(counts: &[u32]) -> Vec<u32> {
    let mut b = vec![0u32; counts.len()];
    let mut sum = 0u32;

    for (i, &c) in counts.iter().enumerate() {
        b[i] = sum;
        sum += c;
    }

    b
}

/// Bucket ends (one-past-the-last slot of each bucket) from symbol counts,
/// for tail-decrement seeding / S-type induction.
fn counts_to_ends(counts: &[u32]) -> Vec<u32> {
    let mut b = vec![0u32; counts.len()];
    let mut sum = 0u32;

    for (i, &c) in counts.iter().enumerate() {
        sum += c;
        b[i] = sum;
    }

    b
}

/// Seeds `sa` with the empty marker, then places each position in `lms` at
/// the tail of its bucket, processed in reverse so that (when `lms` is
/// itself ascending-suffix-sorted) positions land in the correct relative
/// order within a shared bucket. When `lms` is only in left-to-right text
/// order (the first, coarse seeding pass), the specific placement order
/// within a bucket doesn't affect correctness -- SA-IS's induced-sorting
/// theorem guarantees LMS *substrings* come out correctly ordered
/// regardless of how same-bucket LMS entries were seeded.
fn seed_lms(sa: &mut [u32], text: &[u32], bucket_ends: &[u32], lms: &[u32]) {
    sa.fill(u32::MAX);
    let mut bkt = bucket_ends.to_vec();

    for &p in lms.iter().rev() {
        let c = unsafe { *text.get_unchecked(p as usize) as usize };
        let slot = unsafe { bkt.get_unchecked_mut(c) };
        *slot -= 1;
        unsafe {
            *sa.get_unchecked_mut(*slot as usize) = p;
        }
    }
}

fn induce_l(sa: &mut [u32], text: &[u32], is_s: &[u64], bucket_heads: &[u32]) {
    let mut bkt = bucket_heads.to_vec();
    let n = sa.len();

    for i in 0..n {
        let s = unsafe { *sa.get_unchecked(i) };

        if s == u32::MAX || s == 0 {
            continue;
        }

        let j = (s - 1) as usize;

        if !get_bit(is_s, j) {
            let c = unsafe { *text.get_unchecked(j) as usize };
            let slot = unsafe { bkt.get_unchecked_mut(c) };
            unsafe {
                *sa.get_unchecked_mut(*slot as usize) = j as u32;
            }
            *slot += 1;
        }
    }
}

fn induce_s(sa: &mut [u32], text: &[u32], is_s: &[u64], bucket_ends: &[u32]) {
    let mut bkt = bucket_ends.to_vec();
    let n = sa.len();

    for i in (0..n).rev() {
        let s = unsafe { *sa.get_unchecked(i) };

        if s == u32::MAX || s == 0 {
            continue;
        }

        let j = (s - 1) as usize;

        if get_bit(is_s, j) {
            let c = unsafe { *text.get_unchecked(j) as usize };
            let slot = unsafe { bkt.get_unchecked_mut(c) };
            *slot -= 1;
            unsafe {
                *sa.get_unchecked_mut(*slot as usize) = j as u32;
            }
        }
    }
}

/// Compares the LMS substrings starting at `p` and `q` (both LMS
/// positions) for exact equality, including their terminating character.
/// `d==0` (the shared starting offset) never counts as a stopping LMS
/// boundary -- an LMS substring starts at an LMS position but extends
/// *to* the next one, so the start itself doesn't end it.
fn lms_substrings_equal(text: &[u32], is_s: &[u64], p: usize, q: usize) -> bool {
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

        if d > 0 {
            let i_lms = is_lms(is_s, i);
            let j_lms = is_lms(is_s, j);

            if i_lms || j_lms {
                return i_lms && j_lms && text[i] == text[j];
            }
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
/// The returned suffix array still contains the sentinel at index 0.
fn sa_is_core(text: &[u32], k: usize) -> Vec<u32> {
    let n = text.len();

    if n == 1 {
        return vec![0];
    }

    // 1. Classify S-type (1) / L-type (0) suffixes, right to left, in a
    // bitset (n/64 words).
    let mut is_s = vec![0u64; (n >> 6) + 1];
    set_bit(&mut is_s, n - 1);

    for i in (0..n - 1).rev() {
        let si = match text[i].cmp(&text[i + 1]) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => get_bit(&is_s, i + 1),
        };

        if si {
            set_bit(&mut is_s, i);
        }
    }

    // 2. LMS positions, in left-to-right text order.
    let mut lms_positions: Vec<u32> = Vec::with_capacity(n >> 1);

    for i in 1..n {
        if get_bit(&is_s, i) && !get_bit(&is_s, i - 1) {
            lms_positions.push(i as u32);
        }
    }

    debug_assert!(!lms_positions.is_empty(), "sentinel guarantees at least one LMS position");

    // 3. Coarse pass: seed + induce to get LMS *substrings* (not yet full
    // suffixes) correctly ordered relative to each other. Symbol counts are
    // computed once and reused for every bucket pass at this level.
    let counts = bucket_counts(text, k);
    let heads = counts_to_heads(&counts);
    let ends = counts_to_ends(&counts);

    let mut sa = vec![u32::MAX; n];
    seed_lms(&mut sa, text, &ends, &lms_positions);
    induce_l(&mut sa, text, &is_s, &heads);
    induce_s(&mut sa, text, &is_s, &ends);

    // 4. Extract LMS positions from `sa` in their now-correct substring
    // order, and name each distinct substring.
    let mut sorted_lms: Vec<u32> = Vec::with_capacity(lms_positions.len());

    for &x in sa.iter() {
        if x != u32::MAX && is_lms(&is_s, x as usize) {
            sorted_lms.push(x);
        }
    }

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
    let mut sa = vec![u32::MAX; n];
    seed_lms(&mut sa, text, &ends, &final_lms_order);
    induce_l(&mut sa, text, &is_s, &heads);
    induce_s(&mut sa, text, &is_s, &ends);

    sa
}
