# Next steps

Ideas for future performance/fidelity work, not yet started. See
`BENCHMARKS.md` for the investigation history, methodology, and honest
numbers (including negative results) these build on -- in particular the
DivSufSort port, the native-C++ comparison, the level 5-7 stage
profiling, and the SRT/CM/FPAQ bounds-check-elimination passes.

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
   rank shift ported (see `BENCHMARKS.md`, L6 session) -- small win, kept.
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
   16 MiB); details in `BENCHMARKS.md`.

 ## Decode allocation reuse

 Encode reuses per-worker scratch (`Block6Scratch`); decode still builds
 a fresh `Bwt` plus per-stage `vec!`s per block (~90 MiB of fresh pages
 per 8 MiB block, all first-touched under load). Reusing them per worker
 should cut both allocator traffic and the run-to-run timing variance
 decode shows that encode no longer has.

## Housekeeping

 - `python_kanzi/` remains an untracked stray directory in the working
   tree, flagged multiple times this session and never resolved either
   way -- decide whether to delete it or fold it into the project.
 - `BENCHMARKS.md`'s top-of-file 3-way native CLI table (kanzi-rs/
   kanzi-go/kanzi-cpp on `silesia.tar`) predates all of the divsufsort/CM/
   SRT work documented later in that same file. Worth a full re-run once
   more of the above lands, for one coherent up-to-date picture instead of
   piecing it together from several partial sections measured at
   different points in time.
 - `main.rs` CLI `encodeN` commands default to 4 MiB blocks regardless of
   level while kanzi-cpp (and this repo's Python bindings) scale the
   default per level (8 MiB at L6) -- either match the per-level default
   or document the difference; it silently costs ratio at L6+ today.
