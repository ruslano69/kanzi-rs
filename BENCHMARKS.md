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

kanzi-go's own README benchmarks the same corpus (as an actual `.tar`, same
byte size) on an AMD Ryzen 9950X; that table isn't reproduced or compared to
here since the CPU generation, thread count used, and OS all differ enough
to make a row-by-row comparison misleading. See that project's README if
you want the reference-implementation numbers.

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
