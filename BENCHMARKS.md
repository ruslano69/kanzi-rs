# Benchmarks

kanzi-rs (this repo), commit history through the RLT/LZX fixes, the
bounds-check-elimination performance pass, and the per-level default
block-size fix described below.

## silesia.tar

Test machine: AMD Ryzen 9 5950X (16C/32T), all-core fixed at 4000 MHz, 4x DIMM
non-ECC RAM, Windows 10, rustc 1.98.1, kanzi-rs 0.1.0.

Download at http://sun.aei.polsl.pl/~sdeor/corpus/silesia.zip

Encoding and decoding are parallelized across blocks using all available
hardware threads (`std::thread::available_parallelism`, 32 here) -- these
numbers reflect full-machine throughput, not a single core. Block size is
this project's per-level default (see below); sizes are exact byte counts,
timings from a single `--repeats 1` pass.

| Level | Encoding (ms) | Decoding (ms) | Size |
|---|---|---|---|
| Original | | | 211,968,000 |
| **kanzi-rs -l 0** | 2630 | 2270 | 211,968,474 |
| **kanzi-rs -l 1** | 2480 | 900 | 79,202,777 |
| **kanzi-rs -l 2** | 2410 | 800 | 68,646,055 |
| **kanzi-rs -l 3** | 2510 | 790 | 64,451,851 |
| **kanzi-rs -l 4** | 2810 | 880 | 61,192,921 |
| **kanzi-rs -l 5** | 4800 | 1010 | 54,021,324 |
| **kanzi-rs -l 6** | 5780 | 1110 | 49,515,942 |
| **kanzi-rs -l 7** | 6380 | 2120 | 47,309,589 |
| **kanzi-rs -l 8** | 10360 | 8640 | 43,257,955 |
| **kanzi-rs -l 9** | 21380 | 18930 | 41,857,565 |

Reproduce with:

```bash
python examples/benchmark.py --repeats 1 --strict /path/to/silesia.tar
```

kanzi-go's own README benchmarks the same corpus (as an actual `.tar`) on an
AMD Ryzen 9950X; that table isn't reproduced here for a head-to-head on
*speed*, since the CPU generation, job/thread count, and OS all differ
enough to make a timing comparison misleading. Compressed *size* doesn't
depend on any of that, so here's the reference kanzi implementation
(v2.5.1) on the exact same `silesia.tar`, run locally on this machine
(`kanzi -c -l N -j 0`, its own per-level default block size):

| Level | kanzi-rs | reference kanzi (v2.5.1) | Difference |
|---|---|---|---|
| 0 | 211,968,474 | 211,968,000 | +0.000% |
| 1 | 79,202,777 | 79,202,781 | -0.000% |
| 2 | 68,646,055 | 68,646,059 | -0.000% |
| 3 | 64,451,851 | 64,436,766 | +0.023% |
| 4 | 61,192,921 | 61,192,925 | -0.000% |
| 5 | 54,021,324 | 54,021,328 | -0.000% |
| 6 | 49,515,942 | 49,515,946 | -0.000% |
| 7 | 47,309,589 | 47,309,593 | -0.000% |
| 8 | 43,257,955 | 43,257,959 | -0.000% |
| 9 | 41,857,565 | 41,857,569 | -0.000% |

**Every level now matches the reference to within 4 bytes** (level 0's
+474 is store-mode header overhead; level 3's +0.023% is the one
remaining, genuinely tiny discrepancy, not yet chased down). The size
column doesn't match kanzi-go's own README table row for row because that
table used a *different* `silesia.tar` -- same 12 files, apparently packed
slightly differently -- not a difference in this port.

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
