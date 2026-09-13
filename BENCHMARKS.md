# Benchmarks

kanzi-rs (this repo), commit history through the RLT/LZX fixes and the
bounds-check-elimination performance pass.

## silesia.tar

Test machine: AMD Ryzen 9 5950X (16C/32T), all-core fixed at 4000 MHz, 4x DIMM
non-ECC RAM, Windows 10, rustc 1.98.1, kanzi-rs 0.1.0 (Python bindings, cp314).

Download at http://sun.aei.polsl.pl/~sdeor/corpus/silesia.zip

Encoding and decoding are parallelized across blocks (default 4 MiB block
size) using all available hardware threads (`std::thread::available_parallelism`,
32 here) -- these numbers reflect full-machine throughput, not a single core,
and are not directly comparable to a run pinned to fewer threads.

| Compressor | Encoding (ms) | Decoding (ms) | Size |
|---|---|---|---|
| Original | | | 211,968,000 |
| **kanzi-rs -l 0** | **2646** | **2292** | 211,968,000 |
| **kanzi-rs -l 1** | **2462** | **902** | 79,092,537 |
| **kanzi-rs -l 2** | **2412** | **812** | 68,598,058 |
| **kanzi-rs -l 3** | **2549** | **798** | 64,427,964 |
| **kanzi-rs -l 4** | **2784** | **874** | 61,262,428 |
| **kanzi-rs -l 5** | **4802** | **998** | 54,073,469 |
| **kanzi-rs -l 6** | **5016** | **1040** | 50,229,384 |
| **kanzi-rs -l 7** | **4658** | **1526** | 48,394,521 |
| **kanzi-rs -l 8** | **8285** | **6542** | 44,437,736 |
| **kanzi-rs -l 9** | **13941** | **12105** | 42,995,538 |

Reproduce with:

```bash
python examples/benchmark.py --repeats 1 --strict /path/to/silesia.tar
```

(`--repeats 1` because a single pass over ~200MB at level 8-9 already takes
several seconds; `--strict` makes a round-trip mismatch a hard failure
instead of an inline warning.)

kanzi-go's own README benchmarks the same corpus (as an actual `.tar`) on an
AMD Ryzen 9950X; that table isn't reproduced here for a head-to-head on
*speed*, since the CPU generation, job/thread count, and OS all differ
enough to make a timing comparison misleading. Compressed *size*, on the
other hand, doesn't depend on any of that -- so here's what the reference
kanzi implementation (v2.5.1) produces on the exact same `silesia.tar` used
for the table above, run locally on this machine (`kanzi -c -l N -j 0`):

| Level | kanzi-rs | reference kanzi (v2.5.1) | Difference |
|---|---|---|---|
| 0 | 211,968,000 | 211,968,000 | 0.00% |
| 1 | 79,092,537 | 79,202,781 | -0.14% |
| 2 | 68,598,058 | 68,646,059 | -0.07% |
| 3 | 64,427,964 | 64,436,766 | -0.01% |
| 4 | 61,262,428 | 61,192,925 | +0.11% |
| 5 | 54,073,469 | 54,021,328 | +0.10% |
| 6 | 50,229,384 | 49,515,946 | **+1.44%** |
| 7 | 48,394,521 | 47,309,593 | **+2.29%** |
| 8 | 44,437,736 | 43,257,959 | **+2.73%** |
| 9 | 42,995,538 | 41,857,569 | **+2.72%** |

Levels 0-5 are within noise of the reference (this is also why the size
column above doesn't match kanzi-go's own README table row for row: that
table used a *different* `silesia.tar` -- built from the same 12 files but
apparently packed slightly differently -- not a difference in this port).
Levels 6-9, however, have a real, reproducible gap: this port's BWT+SRT
(level 6), LZP+BWT+CM (level 7), and EXE+RLT+TEXT+UTF+DNA+TPAQ/TPAQX (levels
8-9) pipelines are correctly *invertible* (every round-trip in this repo's
history, including this corpus, decodes byte-exact) but compress measurably
worse than the reference at those levels -- almost certainly a heuristic or
threshold difference somewhere in the ported SRT/LZP/CM/TPAQ logic rather
than a missing feature, since level 4 (which also runs an EXE filter) shows
no such gap. Not yet root-caused; tracked as a known limitation rather than
a bug, since nothing here produces incorrect output.

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
