# Benchmarks

kanzi-rs (this repo), commit history through the RLT/LZX fixes, the
bounds-check-elimination performance pass, and the per-level default
block-size fix described below.

## DivSufSort: a native, dependency-free suffix-array backend

This crate's forward BWT (level 5+) needs a suffix array of each block.
`sais.rs` (an from-scratch SA-IS implementation) was the original backend;
the `fast-sa` feature later added libsais (a C library) as a ~2x-faster
opt-in. This section adds a third backend, `divsufsort.rs` -- a mechanical
line-by-line port of kanzi-cpp's `DivSufSort.cpp`/`.hpp` (Yuta Mori's
two-stage induced-sorting algorithm, ~2550 lines, the same algorithm
kanzi-go and kanzi-cpp both use natively) -- and makes it the new default
when the `fast-sa` feature is off, replacing `sais.rs` in that role.
`sais.rs` stays in the tree, now purely as an independent correctness
oracle for `divsufsort.rs`'s own tests.

**Why port DivSufSort at all, given `bwt.rs`'s own long-standing argument
that any correct SA construction works?** That argument is about
*correctness* (all three backends must, and do, produce byte-identical
BWT output), not about speed -- `sais.rs` was always the slower of the
two from-scratch options, and this closes most of that gap without
requiring a C toolchain. Because the port is mechanical (same method
names, same parameter order, same raw-index arithmetic as the C++, no
restructuring), and because `sais.rs`'s and libsais' outputs are proven
byte-identical to each other already, both make cheap, high-confidence
*differential test oracles* -- fuzzed against directly in
`divsufsort.rs`'s own test module (2000+ random small strings against a
naive O(n^2 log n) reference, 300 random strings up to 5000 bytes and 70
cases straddling the algorithm's internal 8192-byte block size against
`sais.rs`, 100 highly-repetitive/periodic strings stressing the
tandem-repeat path, and the full Silesia corpus plus several
multi-megabyte real files against both `sais.rs` and libsais) -- which is
what made porting ~2550 lines of dense offset arithmetic tractable at all
without a Go or C++ debugger to step through side by side.

### Suffix-array construction speed (Silesia corpus, single-threaded, release build)

| File | divsufsort.rs | sais.rs (old default) | libsais (`fast-sa`) |
|---|---|---|---|
| dickens (10MB) | 409ms | 630ms | 189ms |
| mr (10MB) | 370ms | 521ms | 158ms |
| ooffice (6MB) | 192ms | 406ms | 119ms |
| osdb (10MB) | 347ms | 549ms | 201ms |
| reymont (6MB) | 238ms | 311ms | 116ms |
| samba (21MB) | 682ms | 1536ms | 402ms |
| sao (7MB) | 224ms | 457ms | 178ms |
| nci (34MB) | 1169ms | 1486ms | 545ms |
| x-ray (8MB) | 340ms | 490ms | 200ms |
| xml (5MB) | 146ms | 211ms | 75ms |
| webster (41MB) | 2048ms | 3196ms | 904ms |
| mozilla (51MB) | 1942ms | 5627ms | 1103ms |

`divsufsort.rs` is **1.3-2.9x faster than `sais.rs`** on every file above
while adding zero build-time dependencies (pure Rust, no C compiler
needed) -- and every one of these 12 runs produced a byte-for-byte
identical suffix array across all three backends, matching `bwt.rs`'s own
"any correct SA construction yields the same BWT" argument empirically,
not just in theory. libsais remains faster still (roughly another 2x),
which is why `fast-sa` stays the default feature for anyone with a C
toolchain available; `divsufsort.rs` is now what you get without one, in
place of the older, slower `sais.rs`.

### A closer look: is this "identical" port actually as fast as the C++ it was ported from?

The comparisons above are all against *other Rust code* (sais.rs, and
libsais through its Rust bindings) -- none of them answer the more basic
question a mechanical, line-by-line port invites: does it run at the same
speed as the literal C++ it mirrors? It does not, and the gap and its
cause are worth stating plainly rather than leaving implicit.

A standalone harness (`sabench.cpp`, MSVC 14.44 `/O2 /std:c++17`, not
checked into this repo) links kanzi-cpp's actual `DivSufSort.cpp`/`.hpp`
unmodified and times `computeSuffixArray` alone, with no BWT framing, no
entropy coding, nothing else -- the closest possible apples-to-apples
comparison to `divsufsort.rs`'s own `suffix_array`, same machine, same
files:

| File | native C++ DivSufSort | divsufsort.rs | ratio |
|---|---|---|---|
| dickens | 364.9ms | 409ms | 1.12x |
| mr | 351.7ms | 370ms | 1.05x |
| ooffice | 176.5ms | 192ms | 1.09x |
| osdb | 288.4ms | 347ms | 1.20x |
| reymont | 216.6ms | 238ms | 1.10x |
| samba | 615.8ms | 682ms | 1.11x |
| sao | 208.6ms | 224ms | 1.07x |
| nci | 1011.0ms | 1169ms | 1.16x |
| x-ray | 307.5ms | 340ms | 1.11x |
| xml | 129.8ms | 146ms | 1.13x |
| webster | 1885.2ms | 2048ms | 1.09x |
| mozilla | 1789.7ms | 1942ms | 1.09x |

**The Rust port is consistently ~5-20% slower than the native C++ it was
ported from** (aggregate: 8107ms vs 7346ms, 1.10x). An earlier revision of
this section didn't run this specific comparison and, by only comparing
against other Rust backends, left room to misread "faster than sais.rs"
as "as fast as the C++ original" -- it isn't, and there's no reason to
expect a mechanical translation to be: the algorithm is identical, but
what the two compilers are allowed to assume about memory access is not.

**Root cause, confirmed rather than assumed:** every one of DivSufSort's
thousands of `_sa[...]`/`_buffer[...]` dereferences in the C++ is a raw,
unchecked pointer read. Rust's `Index` on a slice inserts a bounds check
at (almost) every one of those sites unless the compiler can prove it
unnecessary, which it generally cannot here -- most indices are computed
from other array reads (`_sa[pa + _sa[x]]`-style double indirection), not
from a simple loop counter LLVM's bounds-check-elimination pass can
reason about. `sais.rs` already documents having deliberately used
`get_unchecked` in its own hot loops for exactly this reason; this port's
first version used none at all. Converting the handful of accessors
actually called in the innermost comparison/pivot/heap loops (`ss_char`,
`ss_char_val`, `tr_char`, `tr_char_val`, and the character-scan loop
inside `ss_compare`/`ss_compare_val`) to `get_unchecked` -- guarded by a
`debug_assert!` so a debug build still catches any violation, safe under
the exact same invariant the original C++ already relies on with *zero*
checking of its own, and re-verified against the entire test suite
(fuzzing included) both before and after -- closed roughly a third of the
gap on its own (16.6% slower -> 10.4% slower in aggregate). The residual
~10% almost certainly has the same root cause spread across the rest of
the module's indexing (every `ss_sort`/`ss_swap_merge`/`tr_intro_sort`
loop body still uses checked indexing), plus whatever remaining share is
ordinary MSVC-vs-LLVM codegen variance for this style of code. Applying
`get_unchecked` module-wide would likely close most of the rest, at the
cost of auditing every remaining index expression's safety instead of
just the six hottest ones -- not done here; call it a documented, tested,
partially-closed gap rather than a mystery.

An end-to-end check confirms the same holds through the full container
pipeline, not just the bare suffix array: encoding the same 8MB real file
at level 6 with `fast-sa` on and off produced byte-for-byte identical
`.knz` output, and both decoded back to the exact original input.

### If SA construction is this fast, why is the full pipeline still slower than kanzi-cpp?

A fair question raised against the numbers above: `fast-sa` (libsais)
builds a suffix array *faster than kanzi-cpp's own native DivSufSort*
(189ms vs 365ms on dickens -- see the table two sections up), yet the
main 3-way table at the top of this file still shows kanzi-rs behind
kanzi-cpp on full-pipeline encode time at levels 1-6. If the sort itself
wins, the remaining loss has to be somewhere else in the pipeline --
otherwise this whole section would have quietly polished a part that was
never the real bottleneck.

Re-instrumented `encode_block5`/`6`/`7` and `Bwt::forward` with temporary
per-stage `Instant` timers (same approach as the level 5-7 profiling
pass in git history, not checked in) and ran them on two real files of
different character (dickens, 10MB English prose; webster, 41MB
dictionary text) at the default `fast-sa` block sizes. Percentages are
each stage's share of total CPU time summed across worker threads:

| Stage | L5 dickens | L5 webster | L6 dickens | L6 webster | L7 dickens | L7 webster |
|---|---|---|---|---|---|---|
| SA construction | 33.7% | 52.5% | 27.9% | 49.3% | 19.6% | 35.8% |
| BWT (rest) | 4.6% | 5.8% | 4.5% | 5.2% | 3.1% | 4.1% |
| TEXT | 20.4% | 17.2% | 16.5% | 13.3% | 10.8% | 9.5% |
| RANK (sbrt.rs) | **32.0%** | 17.7% | -- | -- | -- | -- |
| SRT | -- | -- | **29.3%** | 16.9% | -- | -- |
| ZRLT | 3.2% | 2.6% | 2.5% | 2.0% | -- | -- |
| LZP | -- | -- | -- | -- | 7.5% | 6.4% |
| entropy (ANS0/FPAQ/CM) | 5.6% | 3.5% | 19.0% | 12.5% | **58.7%** | **43.5%** |

SA construction is the single largest line item in every case, but it is
not the *only* large one: at level 5, RANK costs almost as much as the
suffix sort itself; at level 6, SRT costs *more* than the suffix sort;
at level 7, the CM entropy coder alone is 1.2-3x the suffix sort's share
and dominates the whole block. None of RANK (`sbrt.rs`), SRT (`srt.rs`),
FPAQ, or CM have been through anything like the optimization passes BWT
and suffix-array construction got this session (parallel BWT, bounds-
check elimination, libsais, divsufsort.rs) -- so on the current evidence,
**that** is the more likely source of the remaining gap to kanzi-cpp at
levels 1-6, not suffix-array construction, which this section's own
numbers show is already competitive or ahead. Put differently: this
session's DivSufSort work was not spent optimizing a part the sort
"already had covered" -- 20-53% of a block's CPU time is a real cost
center by any measure -- but it does mean further gains from squeezing
suffix-array construction harder are capped at that same 20-53%, while
RANK/SRT/entropy remain untouched and, on this data, comparably or more
expensive. Profiling and optimizing those is the next place to look for
closing the full-pipeline gap, not further suffix-array work.

### SRT: applying the same bounds-check-elimination pass, honestly

`sbrt.rs` (RANK, level 5) already went through a bounds-check-elimination
pass in an earlier commit (`332f7ff`); `srt.rs` (SRT, level 6) never had.
Following the same recipe that worked for `divsufsort.rs` -- converting
the checked `[]` indexing in `forward()`'s two per-byte loops (the
first-symbol/frequency scan, and the main rank-bucket-and-shift loop) to
`get_unchecked`/`get_unchecked_mut`, with every index provably a byte
value (<256), a rank slot (<256), or bounded by `count`/`dst.len()`
(checked once up front) -- and re-verifying the full test suite
(round-trips included) before and after:

**The result is a real but much smaller win than divsufsort.rs's: ~4%**
(measured with `srt.forward()` driven directly by real BWT output from
`webster`, isolated from the rest of the pipeline, before/after with the
same harness: 215.7ms -> 206.5ms). This is worth keeping (it's free,
verified-safe speed with zero output-size change), but it does not come
close to explaining SRT's 17-29% share of block time the way bounds
checks explained roughly a third of divsufsort.rs's gap to native C++.

The likely reason: `ss_char`/`tr_char` in divsufsort.rs sit in a genuinely
memory-bound, cache-unfriendly access pattern (`_sa[pa + _sa[x]]`-style
double indirection through gigantic scratch arrays), where a bounds
check is pure added latency on top of an already-slow load. SRT's hot
loop, by contrast, is a tight *serial dependency chain* over 256-entry
arrays that mostly stay cache-resident: reading `s2r[c]`/`buckets[c]`,
writing one output byte, then shifting up to `r` entries of a 256-slot
rank list where each step's input (`r2s[r-1]`) is only known after the
previous step's output -- a true chain of individually cheap operations
that a CPU cannot reorder or pipeline around no matter how the bounds
checks are removed, since the checks were never the dominant cost to
begin with. (`sbrt.rs`, which does the same style of rank-list
maintenance and was *already* unchecked before this session even
started, is the same story -- its 18-32% share is presumably close to
this same floor already.) Meaningfully beating this would need a
different data structure or algorithm for the rank-list update itself,
not further micro-optimization of the current one -- a larger, riskier
change than anything else done this session, since it risks diverging
from kanzi-go's exact SRT/RANK output ordering if not done with the same
care as everything above it. Not attempted here; flagging it as the
honest ceiling of the low-risk approach instead of overclaiming a bigger
win than the data supports.

### CM: the same pass on the biggest remaining cost center, with a real win this time

Per the profiling above, CM (level 7's entropy stage) is the single
largest cost center measured this session (44-59% of block time) --
bigger than SA construction ever was. Unlike `sbrt.rs`/`srt.rs`,
`cm.rs`'s `CmPredictor::get()`/`update()` had never been touched: every
one of its `counter1`/`counter2` array accesses used plain checked `[]`
indexing. These two methods run once per *bit* (8x per byte, for the
whole block) -- more call volume than anything else profiled this
session -- with 3-4 indexed reads/writes each, so the bounds-check
surface per byte is larger than SRT's.

Converted both to `get_unchecked`/`get_unchecked_mut`: `ctx` (the trie
position within the current byte's 8-bit walk) is provably in `1..=255`
for the whole walk (`update()` resets it to 1 right after it would
exceed 255, before the next byte starts), which bounds every derived
index (`pc1`, `pc2`, plus the `+256`/`+c1`/`+c2`/`+idx`/`+idx+1` offsets)
well inside `counter1`'s and `counter2`'s fixed sizes -- see the safety
comment in `cm.rs` for the exact arithmetic. Full test suite and a real
41MB file's round-trip verified before and after; output size unchanged.

**Result: ~17% faster** (measured the same way as SRT's check -- a
`BinaryEntropyEncoder<CmPredictor>` driven directly by real BWT output
from `webster`, isolated from the rest of the pipeline, before/after with
the same harness: 2288ms -> 1893ms for 3 reps over ~41MB). This is by far
the best return of the three bounds-check-elimination passes done this
session after divsufsort.rs itself (SRT's was ~4%), consistent with the
theory above: more array accesses per call than SRT, on tables (`256*257`
and `512*17` `i32`s, ~1MB combined) too large to stay fully cache-resident
the way SRT's 256-byte arrays do, so each removed check saves more than a
branch-predictor cycle. Still nowhere near the ~30%-of-gap divsufsort.rs
saw against native C++, since CM's tables are far smaller than
divsufsort's multi-megabyte scratch arrays and its access pattern has
much better locality (`ctx` walks a fixed 8-level trie per byte, not an
arbitrary computed offset) -- there's simply less latency for a bounds
check to hide behind here than in DivSufSort's double indirection.

## silesia.tar

Test machine: AMD Ryzen 9 5950X (16C/32T), all-core fixed at 4000 MHz, 4x DIMM
non-ECC RAM, Windows 10, rustc 1.98.1, kanzi-rs 0.1.0.

Download at http://sun.aei.polsl.pl/~sdeor/corpus/silesia.zip

All three implementations below are native CLI binaries (this repo's
`rust_kanzi.exe`, kanzi-go v2.5.1's `kanzi.exe`, and kanzi-cpp's `Kanzi64.exe`
built locally from its `msvc/Kanzi_VS2022.sln` with MSVC 14.44/Release/x64),
run directly (`-c`/`-d -l N -j 0`, i.e. all 32 threads, each project's own
per-level default block size) on the exact same `silesia.tar` on this one
machine -- no Python involved. See the note below on why that last part
matters.

| Level | kanzi-rs enc/dec (ms) | kanzi-go enc/dec (ms) | kanzi-cpp enc/dec (ms) | kanzi-rs size | kanzi-go size | kanzi-cpp size |
|---|---|---|---|---|---|---|
| 1 | 419 / 218 | 319 / 191 | 219 / 118 | 79,202,777 | 79,202,781 | 79,202,781 |
| 2 | 349 / 254 | 259 / 208 | 204 / 126 | 68,646,055 | 68,646,059 | 68,646,059 |
| 3 | 471 / 265 | 411 / 238 | 274 / 145 | 64,451,851 | 64,436,766 | 64,436,766 |
| 4 | 721 / 420 | 771 / 354 | 386 / 216 | 61,192,921 | 61,192,925 | 60,738,928 |
| 5 | 2761 / 616 | 1480 / 665 | 1246 / 507 | 54,021,324 | 54,021,328 | 54,021,328 |
| 6 | 3799 / 744 | 1836 / 866 | 1774 / 944 | 49,515,942 | 49,515,946 | 49,515,946 |
| 7 | 4154 / 1785 | 2725 / 4126 | 2675 / 4596 | 47,309,589 | 47,309,593 | 47,309,593 |
| **8** | **8523 / 8702** | 11843 / 12057 | 10149 / 7970 | 43,257,955 | 43,257,959 | 43,261,199 |
| **9** | **20257 / 20812** | 22487 / 27791 | 25308 / 22860 | 41,857,565 | 41,857,569 | 41,857,569 |

**Every level matches at least one reference implementation to within a
handful of bytes.** kanzi-rs and kanzi-go agree almost exactly everywhere
except level 3 (+0.023%, still unexplained, likely a tiny difference
somewhere in PACK/MM/LZX or Huffman table-building); kanzi-cpp's own
numbers at levels 3, 4 and 8 diverge slightly from *both* Go and Rust (most
visibly at level 4, ~0.7% smaller) -- a reminder that "the reference" isn't
perfectly bit-identical across kanzi-go and kanzi-cpp either, so kanzi-rs
matching one of them almost exactly is the realistic bar, not matching
all three simultaneously.

**Speed**: at levels 1-6 kanzi-cpp is fastest across the board (expected --
it's the oldest, most hand-tuned implementation of the three), with
kanzi-rs 1.1-2.6x slower than kanzi-cpp and roughly in line with
kanzi-go. At levels 8-9, though, **kanzi-rs is the fastest of the three**
on both encode and decode -- this repo's block-level parallelism
(`std::thread::scope` across all 32 threads, see git log) and bounds-check
elimination work paid off specifically where the adaptive entropy coders
make it matter most. Levels 1-6 haven't had the same optimization pass and
are the more promising target if raw speed at low levels matters to you.

### A note on benchmarking through the Python bindings instead of the CLI

An earlier revision of this table was timed through `kanzi.compress()`
(Python) instead of the CLI directly, and looked *much* worse at low
levels -- level 1 measured at ~2.5 **seconds** through Python versus 319ms
for kanzi-go's CLI on the same machine, an apparent 8x regression. It
wasn't algorithmic: `compress()` copies the input `bytes` into a Rust
`Vec<u8>` and copies the output back out as a new Python `bytes` object,
and that fixed marshaling cost (a few hundred ms for a 202MB buffer) is
*constant* regardless of level, so it swamps every timing at level 1 (a
few hundred ms of real work) while being negligible at level 9 (twenty
seconds of real work). Re-timing through the native CLI binary (no Python
in the loop) is what produced the table above and matches expectations.
If you're benchmarking this port, prefer the CLI for anything level 6 or
below, or expect a roughly-fixed few-hundred-ms-per-call Python/copy tax on
top of the real work at those levels.

Reproduce the CLI numbers with `cargo build --release` and time
`rust_kanzi.exe encodeN`/`decode` directly; `examples/benchmark.py` (Python)
remains useful for round-trip correctness (`--strict`) and for levels 7-9
where the marshaling cost is a rounding error.

kanzi-go's own README benchmarks the same corpus (as an actual `.tar`) on
an AMD Ryzen 9950X; that table isn't reproduced here since the CPU
generation differs enough from this machine's 5950X to make a direct
timing comparison pointless on top of everything above -- the point of
this section is the three-way, same-machine comparison, not matching that
table's numbers.

### Postmortem: the block-size bug that looked like a TPAQ bug

Earlier revisions of this section reported a real, reproducible gap growing
from +1.5% (level 6) to +2.8% (levels 8-9), worse on text-heavy files
(webster, a dictionary, was the worst outlier at +10.24% on its own) and
invisible on binary ones. That pointed hard at the adaptive entropy coders
(FPAQ/CM/TPAQ/TPAQX, the stages unique to those levels) rather than the
transforms feeding them, and cost a long investigation: a full line-by-line
audit of `TPAQPredictor.go` against `tpaq.rs` (every static table diffed
programmatically byte-for-byte, `LogisticApm`, the shared arithmetic coder,
both context branches of `update()`), a cross-check against kanzi-cpp's
independent C++ implementation, and disproving an initial (wrong) guess
that suffix-array choice was involved -- swapping the SA-IS backend for
the `fast-sa` feature's libsais changes nothing at any level, exactly as
expected for a mathematically unique suffix order. That audit did turn up
one real, confirmed bug (`TpaqMixer::get()`'s dot product used `i64`
instead of Go's wrapping `int32` -- fixed, see git log) but fixing it
changed nothing on this corpus, because it wasn't the cause.

The actual cause: this crate's Python bindings (`lib.rs`) hardcoded a 4 MiB
block size for every level. kanzi-go's `BlockCompressor` doesn't -- it
scales the default block size with level (`app/BlockCompressor.go`):
levels 0-5 use 4 MiB, level 6 uses 8 MiB, levels 7-8 use 16 MiB, and level 9
uses 32 MiB, specifically so the adaptive models get more data per block
before their next reset. Every benchmark and comparison run through this
repo's Python API had been comparing this port at a *forced* 4 MiB against
the reference at its *real, level-scaled* default -- smaller blocks meaning
more frequent model resets, smaller internal hash/dictionary tables, and
measurably worse compression, entirely unrelated to any algorithmic
difference. Confirmed by isolating a 1 MiB prefix of webster: dumping the
post-transform, pre-entropy bytes from both implementations showed they
were *already* different at 4 MiB (850,884 vs 851,572 bytes) -- and
re-running this port with `block_size=16_777_216` (level 8's real default)
instead of the hardcoded 4 MiB made that difference disappear completely,
byte for byte.

Fixed in `lib.rs`: `compress()` now defaults to the same per-level block
size kanzi-go uses, with an optional `block_size` keyword argument to
override it (matching the CLI's `-b`/`--block`) for anyone who wants a
specific block size regardless of level.

### A note on hardware stability at this scale

While putting this benchmark together, an early run at this machine's default
boosted clocks (and again at an all-core 4275 MHz fixed overclock) hit a rare
decode mismatch at level 9 (TPAQX) -- `Binary entropy codec: Invalid
bitstream` -- on this same 202MB input. It did not reproduce across 30+
follow-up single-block and single-level retries at the same clocks, nor
across 8 consecutive full compress+decompress cycles once the clock was
lowered to 4000 MHz (this table's setting). Sustained all-core TPAQX
encode/decode is about as demanding a load as this codebase produces, so in
effect it doubles as a Prime95/OCCT-style stability stress test -- with the
advantage that a bad answer is unambiguous rather than needing a separate
verifier: decode either reproduces the input exactly or it doesn't.
This pattern (frequency-sensitive, non-reproducible on isolated retries,
absent on non-ECC-adjacent lower clocks) points at marginal CPU/memory
stability under sustained full-core load on this specific machine's
overclock, not a kanzi-rs bug -- but if you're compressing large blocks at
level 8/9 on hardware you haven't stability-tested this hard before,
consider enabling a block checksum (this project supports `-x32`/`-x64` at
the container level; the Python bindings don't expose it yet) so that kind
of corruption is caught as a clean error instead of passing silently.
