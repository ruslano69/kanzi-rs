# Next steps

Ideas for future performance/fidelity work, not yet started.
`BENCHMARKS.md` stays a clean kanzi-rs/kanzi-go/kanzi-cpp benchmark
record; the investigation history behind the items below (what was
tried, what measured as a real win, what didn't and why) lives in git
log instead.

## Closing the gap to native C++ further

- **TPAQ/TPAQX (`tpaq.rs`, levels 8-9)**: never directly benchmarked
  against kanzi-cpp's native `TPAQPredictor` in isolation, unlike
  CM/DivSufSort. kanzi-rs already beats kanzi-go at these levels
  (parallel BWT + bounds-check elimination), but that says nothing about
  where it stands against kanzi-cpp specifically. Repeat the
  `sabench.cpp`-style approach used for DivSufSort: a minimal standalone
  C++ program linking kanzi-cpp's `TPAQPredictor.cpp` directly, timing
  `Get()`/`Update()` in isolation, same real files. If it shows a real
  gap and the cost is checked-indexing (the same shape of finding as
  `cm.rs`), apply the same `debug_assert!`-guarded `get_unchecked` pass;
  if the compiler already optimizes it (the `fpaq.rs` outcome), say so
  and stop.

- **`divsufsort.rs`'s residual ~10% gap to native C++**: only 6 of the
  module's ~35 methods (`ss_char`, `ss_char_val`, `tr_char`,
  `tr_char_val`, `ss_compare`, `ss_compare_val`) got `get_unchecked`. The
  rest (`ss_sort`, `ss_swap_merge`, `ss_multi_key_intro_sort`,
  `tr_intro_sort`, etc.) still use checked `self.sa[i as usize]`
  indexing throughout. Extending the same treatment module-wide would
  likely close more of the remaining gap, but the larger surface area
  means more chances to get an index expression's safety proof wrong --
  convert one function at a time and re-run the full fuzz/differential
  test suite (already in place: 2000+ random strings, the Silesia
  corpus, several real multi-MB files against `sais.rs`/libsais) after
  each, not all at once.

## RANK/SRT: the harder, riskier option

Bounds-check elimination hit its ceiling on SRT (~4%) and RANK was
already at that ceiling (optimized in an earlier session). Both are
serial dependency-chain rank-list updates where each step depends on the
previous step's output -- a real further speedup needs a different
underlying data structure/algorithm for the rank-list itself, not more
micro-optimization of the current one. This is a bigger, riskier change
than anything done so far: any behavioral deviation risks producing a
different-but-still-plausible rank ordering instead of kanzi-go's exact
one, which would silently break decode compatibility with real
kanzi-go/kanzi-cpp bitstreams rather than just failing loudly. Not
attempted. If pursued, it needs the same fuzzing rigor `divsufsort.rs`
got -- byte-for-byte comparison against real kanzi-go output on many
real files, not just this crate's own round-trip tests -- before trusting
it.

## Decode-side symmetry

 This session's bounds-check-elimination passes (SRT, CM) only touched
 the encode/forward path.

 - CM's predictor is shared between encoder and decoder (`get()`/
   `update()` are the same code either way via the generic `Predictor`
   trait), so decode already benefits automatically -- nothing to do
   there.
 - SRT's `inverse()` (decode) has since had kanzi-cpp's `r <= 8` unrolled
   rank shift ported -- small win, kept.
 - BWT inverse dominates L6 decode (~70%/block) and kanzi-cpp beats this
   port there by 1.5-3x on the wall clock via a persistent thread pool +
   chunk fan-out over BiPSIv2 -- while our sequential MergeTPSI measures
   at its single-threaded parity. A scoped fan-out of either walk was
   tried and measured strictly worse (spawn storms under load; plus an
   MLP argument why chain-splitting can't help the MergeTPSI walk at
   all). If chunk-level decode parallelism is ever revisited, it needs a
   persistent pool like kanzi-cpp's `_pool`, not per-block scoped
   threads. Porting BiPSIv2 itself was tried, verified byte-exact, and
   reverted (slower-or-tied single-threaded at every size through
   16 MiB) -- see "Decode thread pool" below for the follow-up session
   that revisited this and actually ran it in parallel.
 - Extended to L5/L7 in a later session: BWT dominance is data-dependent, not per-level -- it holds on
   highly redundant input (~88%/~29% of the block at L5/L7 respectively)
   but RANK (L5) and especially CM entropy decode (L7, up to 59% on a
   less-redundant synthetic file) can rival or exceed it. Found and kept a
   real, small win applicable to L7/8/9: `binary_entropy.rs`'s `read()`
   refill (the one checked-indexing site left in the CM/TPAQ/TPAQX shared
   decoder driver, `cm.rs`'s predictor itself already having been done)
   got the same `debug_assert!`+`get_unchecked` treatment, ~1% on CM
   decode, verified byte-identical. Tried the same treatment on
   `text_codec.rs`/`text_codec1.rs`'s per-word hash loop (a real
   contributor at both L5 and L7) and reverted it -- no measurable gain,
   same shape of negative result as the `fpaq.rs` and BWT-walk attempts:
   the loop is too small a fraction of the per-word cost (dictionary
   lookup/insert dominates, not the hash) for the already-cheap bounds
   check to matter.
 - TPAQ/TPAQX's own predictor `get()`/`update()` (heavier than CM's --
   SSE stage, hashed contexts) has never been checked for the same
   bounds-check opportunity CM's `get()`/`update()` got; CM entropy
   decode's newly-measured weight at L7 makes this more likely to matter
   than the L6-only profiling suggested. Not attempted this session.
 - TEXT decode's dictionary lookup/insert path (`dict_map`/`dict_list`
   indexing in `text_codec.rs`/`text_codec1.rs`) is where the real
   per-word cost actually lives (the hash loop tried above wasn't it) --
   worth a look if TEXT decode is revisited, but its index derivations are
   less local than the hash loop's and would need the same
   one-function-at-a-time rigor `divsufsort.rs`'s remaining checked
   functions call for, not a quick pass.

## Decode thread pool: two changes landed, one path still open

Follow-up session, on branch `decode-thread-pool`, picked up the
"persistent pool" idea two paragraphs up. Did the cheap test first
instead of jumping straight to a pool, then let its result change the
plan for the expensive one. Both are kept.

**1. Work-stealing over a static block split.**
`decode_blocks_parallel`'s per-call `std::thread::scope` was handing
every worker a **static** contiguous range of block indices, which only
balances load if every block costs the same to decode -- false in
general (block cost is content-dependent, per the CM-entropy numbers
above). Replaced with work-stealing over one shared `AtomicUsize` cursor
(no new pool primitive, no `unsafe`). **~35-40% faster wall clock** on an
adversarial mixed-content file, byte-identical output, no regression on
a matched-cost control.

**2. BiPSIv2, re-ported and actually run in parallel this time.** The L6
session's BiPSIv2 attempt only ever compared it *single-threaded*
against single-threaded MergeTPSI (found it slower, reverted) -- the
comparison that actually matters, parallel BiPSIv2 against sequential
MergeTPSI, had never been made. Re-ported from kanzi-cpp's current
source (the old port wasn't in git history), this time with a fully safe
disjoint-slice design (`split_at_mut`, no `unsafe`) instead of the
original's shared-buffer-plus-implicit-disjointness-proof, and validated
by a differential suite across every thread count 1-8 (which caught two
real transcription bugs a single-threaded-only comparison never would
have: kanzi-cpp's `_buffer` needs `count + 1` slots not `count`, and its
sequential tail loop's deliberate one-byte overshoot on odd `ck_size` --
harmless in a shared buffer, harmful against a disjoint slice -- needed
an explicit skip). Result: **BiPSIv2 is simply a faster algorithm at
real block sizes, even single-threaded** (this session's 3-content-type
sweep shows content, not just size, decides the L6 session's "ties at 16
MiB" finding -- not a contradiction, a different point on the same
curve). Threading adds a further 3-8% and stops paying past 4-6 threads
(memory-bandwidth-bound, not spawn-cost-bound, on this machine). Shipped
**single-threaded BiPSIv2** as the decode dispatch above an empirically-
chosen `BIPSI_THRESHOLD` (8 MiB, chosen against the *worst* measured
case -- incompressible content -- not the best): L6/L7's default block
sizes now use it, L5 (4 MiB) still uses MergeTPSI since BiPSIv2 loses by
up to 27% there on incompressible content. Also incidentally lifts
`inverse_merge_tpsi`'s 16 MiB hard block-size ceiling (BiPSIv2 has none)
for anyone passing a custom block size above it.

**Still open: multi-threaded BiPSIv2 fan-out.** The single-block gap
this closes is real (~5-11% end-to-end on the measured files) but
threading's own further 3-8% was left unwired, not because it doesn't
work (it measurably does) but because capturing it
needs lending a work-stealing worker's currently-idle peers to one large
block's chunk fan-out without double-booking threads the block-level
scheduler already owns -- a task-stealing scheduler with two
granularities (block-level, chunk-level), real added complexity for a
3-8% top-up on a win that's already banked. If revisited: the algorithm
side is already done (`Bwt::inverse_bipsiv2` takes a `num_threads`
parameter today, just always called with `1`); what's missing is purely
the scheduling integration, most naturally in the "few large blocks,
many idle cores" case (`spans.len() < workers`) where today's
block-level work-stealing has nothing to steal in the first place.
`BIPSI_THRESHOLD` itself is also only sweep-tuned on synthetic content
(random/text-like/repetitive) on one machine -- a real corpus would pin
it down more precisely.

## Scratch reuse (retired)

Per-worker scratch reuse was tried on both encode (`Block6Scratch`) and
decode (`DecodeScratch`, persistent `Bwt` + ping-pong stage buffers) and
both were reverted after interleaved ablation: on this workload the
system allocator already recycles per-block buffers efficiently, so
reuse bought nothing and keeping large buffers live across the worker's
lifetime (plus the copy a ping-pong buffer needs on output) cost a
little.

## Housekeeping

 - `python_kanzi/` remains an untracked stray directory in the working
   tree, flagged multiple times this session and never resolved either
   way -- decide whether to delete it or fold it into the project.
 - ~~`BENCHMARKS.md`'s 3-way native CLI table was stale, predating most
   of this file's own work~~ -- resolved: re-run on current code (two
   machines, including a fresh i3-12100 pass after the Huffman/LZX/BiPSIv2
   work below), file trimmed down to just that benchmark record.
 - ~~`main.rs` CLI `encodeN` commands default to 4 MiB blocks regardless of
   level~~ -- resolved: `main.rs` now mirrors `lib.rs::default_block_size`
   (4/8/16/32 MiB by level).

## L2/L3 decode: two real wins, one still-open gap

Same session as above, different target: re-running the silesia.tar
3-way comparison on a second (much weaker, 4C/8T) machine showed the
*relative* gap to kanzi-cpp is actually largest at L1-L3 (2-4x), not
L5-9 -- those levels never got an optimization pass. L3
(`TEXT+UTF+PACK+MM+LZX&HUFFMAN`) was the target; the three-attempts-out-
of-three TEXT failure analysis (bounds-check elimination, a bulk-copy
restructuring, porting kanzi-cpp's char-type table -- all reverted,
verified via interleaved A/B) lives in git log, not here or in
`BENCHMARKS.md`.

- **Kept**: LZX's `dist == 1` match-copy special case (ported from
  kanzi-cpp's `memset`, this port previously fell through to a generic
  byte-loop) -- small, real, content-dependent win (~6-7% on RLE-heavy
  content, near-neutral on silesia's actual mix).
- **Kept, the real win**: Huffman decode's guard-byte buffer slack
  (ported from kanzi-cpp; this port's kanzi-go-derived buffer sizing had
  no such margin, paying for a bounds-checked zero-padded copy on every
  bit-refill instead) -- **~20% faster on the entropy stage itself**,
  verified against 1000 corrupted-input fuzz trials (zero panics). Also
  picked up kanzi-cpp's `maxFragBits` sanity check along the way, a real
  robustness gain this port's kanzi-go-derived code lacked.
- **Reverted x3**: TEXT decode resisted bounds-check elimination, a
  bulk-copy restructuring (measured *worse* -- iteration count isn't
  iteration cost), and porting kanzi-cpp's combined char-type table
  (measured worse too, once interleaved-A/B'd properly -- a single
  noisy first measurement had suggested a win). TEXT's real per-word
  cost is still unlocated; next time, instrument the dictionary
  lookup/insert path directly instead of guessing from loop structure.
- **Still open**: Huffman's 20%-on-stage win barely moved whole-decode
  wall time (diluted across LZX/TEXT/MM/PACK sharing the same 8-thread
  pool) -- TEXT (40% of L3 decode) and LZX (23%) still need their own
  wins for the wall-clock number to actually move. Worth checking this
  port's *other* kanzi-go-derived per-call bit/byte-refill sites for the
  same "kanzi-go pads per call, kanzi-cpp reserves buffer slack instead"
  pattern that paid off for Huffman, before assuming it was a one-off.
