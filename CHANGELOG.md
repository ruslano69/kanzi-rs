# Changelog

## 0.2.1 — 2026-09-22

- Huffman encoder: the `limit_code_lengths` slow path now renormalizes
  frequencies (scale 2048) and recomputes code lengths instead of falling
  back to flat 8-bit codes, matching kanzi-cpp; plus unit tests covering
  the fallback.
- Python distribution renamed to `kanzi` and published to PyPI
  (`pip install kanzi`); release wheels + sdist are built by
  `.github/workflows/publish.yml` via Trusted Publishing.

## 0.2.0 — 2026-09-16

Decode at levels 1–4, where kanzi-cpp used to be 1.7–3.4x faster, is now
within 2% of it either way; levels 5–8 moved from roughly even to a 3–29%
lead, and level 9 stayed 2% behind. Peak memory no longer scales with
input size.
Bitstream output is unchanged except at level 4, where a fixed opcode
constant makes it smaller. See [BENCHMARKS.md](BENCHMARKS.md) for the
full three-way table.

### Compression ratio

- **EXE transform: the CBNZ opcode constant was `0x03500000` instead of
  `0x35000000`**, so ARM64 branches were undercounted and the transform
  declined to apply where kanzi-cpp applies it. Level 4 on silesia.tar
  now compresses 0.74% better (61,162,513 → 60,710,689 bytes) and matches
  kanzi-cpp within four bytes at levels 4, 8 and 9.

### Memory

Peak commit used to grow with the file; it is now bounded by the block
size and the number of workers.

- Streaming decoder and encoder: blocks are read, processed and written
  one at a time through a shared ordered-emission engine, instead of
  loading the whole input and building the whole output in memory.
  Level 0 encode of silesia.tar: 791 → 89 MB, 433 → 173 ms.
- BWT inverse runs in place, dropping a second full-block buffer
  (−43 MB at level 5, −61 at 6, −85 at 7).
- A block's compressed bytes are freed after the entropy stage, and the
  entropy output after the first inverse transform.
- Decode recycles block buffers through a per-call pool with a byte
  budget rather than allocating each block fresh.

### Decode speed

- **ANS**: branchless renormalization and 16-bit symbol table entries,
  mirroring kanzi-cpp. Order-1 is now 24.0 ms against kanzi-cpp's 31.2 on
  a 9.7 MB payload, order-0 18.2 against 22.3.
- **ROLZ** and **TEXT**: literals, matches and dictionary words emitted
  in whole 16- and 8-byte pieces instead of exact-length copies.
- **LZX**: 16-byte wide literal copies.
- **Huffman**: the per-symbol shift guard is gone from the table lookup.
- **BWT**: BiPSIv2 inverse.
- `BitReader::read_array` moves 8 bytes at a time when unaligned.
- Block scheduling is work-stealing rather than a static split, worth
  35–40% on files whose blocks compress unevenly.
- The CLI writes output in 4 MiB chunks: a single large `WriteFile` is
  4–5x slower on Windows.

### Encode speed

- LZX `hash()`/`find_match()` use unchecked reads (3–6% at level 1).
- CM `get()`/`update()` bounds-check elimination (~17%).
- SRT forward loops (~4%).
- DivSufSort ported from kanzi-cpp and made the default suffix-array
  backend, so no C toolchain is needed.

### Packaging

- **Split into a Cargo workspace**: `kanzi` (pure Rust, no pyo3),
  `kanzi-cli`, `kanzi-py`. Plain `cargo build` and `cargo test` no longer
  need a Python interpreter; the extension module is built by maturin or
  explicitly with `cargo build -p kanzi-py`.
- New streaming API: `kanzi::compress_to` and `kanzi::decompress_to`
  take any `Read`/`Write`. `compress`/`decompress` are unchanged.
- Python binding takes input zero-copy and releases the GIL for the
  duration of the call: compress 2.3–2.8x faster, decompress −23 ms of
  fixed per-call cost.
- `KANZI_JOBS` sets the worker count for the CLI.

### Tests

- `ans.rs` had no tests; it now has a round-trip across both orders,
  sizes spanning the raw-copy cutoff and the odd tail, several chunk
  sizes and single-symbol chunks, plus a corruption sweep.
- New coverage for `bitio`'s array fast paths, the in-place BWT inverse,
  LZX round-trips with output slack, the EXE ARM64 detector, and
  wide-copy paths in TEXT and ROLZ.

### Compatibility

Bitstream v7, byte-compatible with kanzi-cpp 2.5.3 in both directions
(verified over edge inputs, 32- and 64-bit checksums, and silesia.tar at
every level). kanzi-go 2.5.1 writes v6 only and cannot exchange streams
with either; reading v6 is not supported yet.

## 0.1.0 — 2026-09-13

First release: full level 0–9 pipeline, bitstream v7, Python binding.
