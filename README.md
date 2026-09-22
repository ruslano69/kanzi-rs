# kanzi-rs

A from-scratch Rust port of [Kanzi](https://github.com/flanglet/kanzi-go) — Frederic
Langlet's lossless data compressor — plus Python bindings (via [PyO3](https://pyo3.rs))
that expose it as a normal importable `kanzi` module.

This is an independent, unofficial project. It is not affiliated with or endorsed by
the kanzi-go project. See [Attribution & license](#attribution--license).

## What's here

A Cargo workspace with three crates:

- `crates/kanzi` — the codec itself, pure Rust with no Python dependency: bitstream
  I/O, the BWT/suffix array construction, LZ/LZX/ROLZ, RLT/ZRLT, rank/MTF transforms,
  the TEXT/UTF/EXE/DNA filters, and the entropy coders (Huffman, ANS0, FPAQ, CM,
  TPAQ/TPAQX) that back Kanzi's compression levels 0-9. `kanzi::compress` /
  `kanzi::decompress` cover the whole container.
- `crates/kanzi-cli` — the `rust_kanzi` binary, for encoding/decoding files and
  cross-checking the port against the real kanzi-go/kanzi-cpp binaries (see `verify/`).
- `crates/kanzi-py` — the PyO3 extension module (built as `kanzi` by
  [maturin](https://www.maturin.rs/)), a thin wrapper over `crates/kanzi`.
- `crates/kanzi-py/examples/benchmark.py` — density/speed-by-level benchmark, run
  against real files already in this repo rather than synthetic data (see below).

Plain `cargo build` / `cargo test` build and test only the pure-Rust crates; the
Python module is built by maturin.

## Compression levels

Same level numbering and transform/entropy combinations as kanzi-go:

| Level | Transforms | Entropy |
|---|---|---|
| 0 | (store) | none |
| 1 | LZX | none |
| 2 | DNA+LZ | Huffman |
| 3 | TEXT+UTF+PACK+MM+LZX | Huffman |
| 4 | TEXT+UTF+EXE+PACK+MM+ROLZ | none |
| 5 | TEXT+UTF+BWT+RANK+ZRLT | ANS0 |
| 6 | TEXT+UTF+BWT+SRT+ZRLT | FPAQ |
| 7 | LZP+TEXT+UTF+BWT+LZP | CM |
| 8 | EXE+RLT+TEXT+UTF+DNA | TPAQ |
| 9 | EXE+RLT+TEXT+UTF+DNA | TPAQX |

Higher levels generally compress better at the cost of speed, but (as in kanzi-go)
this isn't guaranteed for every input — see `examples/benchmark.py`'s output.

## Building the CLI

```bash
cargo build --release
./target/release/rust_kanzi encode8 input.bin output.knz
./target/release/rust_kanzi decode output.knz restored.bin
```

Suffix array construction uses [libsais](https://github.com/IlyaGrebnov/libsais) via
the default `fast-sa` feature, which needs a C compiler at build time. Without one,
build the pure-Rust fallback (a port of kanzi-cpp's DivSufSort):

```bash
cargo build --release --no-default-features
```

## Building the Python module

Requires [maturin](https://www.maturin.rs/) (`pip install maturin`):

```bash
maturin build --release
pip install target/wheels/kanzi_rs-*.whl
```

Or, for local development (builds in place, reinstalls on every `maturin develop`):

```bash
pip install maturin
maturin develop --release
```

```python
import kanzi

data = open("input.bin", "rb").read()
compressed = kanzi.compress(data, 6)      # level 0-9
restored = kanzi.decompress(compressed)
assert restored == data

# Block size defaults to the same per-level value the reference kanzi CLI
# uses (4 MiB for levels 0-5, 8 MiB for level 6, 16 MiB for levels 7-8,
# 32 MiB for level 9); override it explicitly if you want a fixed size
# regardless of level, matching the CLI's -b/--block:
compressed = kanzi.compress(data, 8, block_size=4 * 1024 * 1024)

kanzi.compress_to_file("input.bin", 6)    # writes input.bin.kanzi
kanzi.decompress_to_file("input.bin.kanzi")
```

The wheel is built per-interpreter (not `abi3`), so it needs building against
whichever CPython version you're targeting.

### Benchmarking

```bash
python crates/kanzi-py/examples/benchmark.py
```

Reports compressed size, ratio and encode/decode throughput per level against real
files already in the repo (its own docs, source code, the compiled CLI binary, a
built wheel) plus a random-bytes incompressible baseline — deliberately not
synthetic/repetitive data, since that's what caught a real encoder bug during
development (see git log).

See [BENCHMARKS.md](BENCHMARKS.md) for numbers against the Silesia corpus, and
[CHANGELOG.md](CHANGELOG.md) for what changed between releases.

## Status

Levels 0-9 round-trip correctly against both a large real-world corpus and the
reference kanzi-go/kanzi-cpp binaries used for cross-checking in `verify/`. This is
still a young port: if you find a mismatch, please open an issue with the input that
triggers it.

## Attribution & license

kanzi-rs is a derivative work of [kanzi-go](https://github.com/flanglet/kanzi-go)
(Copyright 2011-2026 Frederic Langlet), which is licensed under the
[Apache License, Version 2.0](LICENSE). This project is licensed under the same
terms; see [LICENSE](LICENSE).
