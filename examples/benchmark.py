#!/usr/bin/env python3
"""Benchmark the `kanzi` extension module: density (ratio) and speed per level.

Usage:
    python examples/benchmark.py                 # use real files from this repo
    python examples/benchmark.py file1 file2...   # benchmark specific files

With no arguments, benchmarks real, representative content already sitting in
this repo instead of made-up data:
  - a random-bytes baseline (the incompressible floor every codec must respect)
  - the project's own Markdown docs (real English/technical prose)
  - the Go source this Rust port is based on (real, mixed-content source code)
  - the compiled kanzi.exe (real native binary / machine code)
  - the built Python wheel (a real zip archive - already compressed data)
Any of these that isn't present on disk is skipped rather than faked.
"""
import os
import pathlib
import sys
import time

import kanzi

LEVELS = range(10)
REPEATS = 3  # keep the fastest of REPEATS runs per level to cut noise


def human(n: float) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024:
            return f"{n:.1f}{unit}"
        n /= 1024
    return f"{n:.1f}TB"


def real_datasets(rust_root: pathlib.Path) -> list[tuple[str, bytes]]:
    """Collect real files already present in the repo, skipping what's missing."""
    repo_root = rust_root.parent
    datasets: list[tuple[str, bytes]] = [
        ("random bytes (incompressible baseline)", os.urandom(2 * 1024 * 1024))
    ]

    docs = [repo_root / "README.md", repo_root / "OPTIMIZATIONS.md"]
    docs = [p for p in docs if p.exists()]
    if docs:
        blob = b"".join(p.read_bytes() for p in docs)
        datasets.append((f"project docs ({len(docs)} .md file(s), real prose)", blob))

    go_dir = repo_root / "v2"
    go_files = sorted(go_dir.rglob("*.go")) if go_dir.exists() else []
    if go_files:
        blob = b"".join(p.read_bytes() for p in go_files)
        datasets.append((f"Go source code ({len(go_files)} real files)", blob))

    exe_path = rust_root / "verify" / "kanzi.exe"
    if exe_path.exists():
        datasets.append(("native binary (kanzi.exe, real machine code)", exe_path.read_bytes()))

    wheels = sorted((rust_root / "target" / "wheels").glob("*.whl"))
    if wheels:
        datasets.append((f"compressed archive ({wheels[-1].name}, real zip)", wheels[-1].read_bytes()))

    return datasets


def benchmark(name: str, data: bytes) -> None:
    print(f"\n=== {name}  ({human(len(data))}) ===")
    header = f"{'level':>5} | {'size':>10} | {'ratio':>6} | {'saved':>7} | {'enc MB/s':>9} | {'dec MB/s':>9}"
    print(header)
    print("-" * len(header))

    mib = 1024 * 1024
    for level in LEVELS:
        best_enc = best_dec = None
        compressed = b""
        mismatch = False
        for _ in range(REPEATS):
            t0 = time.perf_counter()
            compressed = kanzi.compress(data, level)
            t1 = time.perf_counter()
            restored = kanzi.decompress(compressed)
            t2 = time.perf_counter()
            if restored != data:
                mismatch = True
            best_enc = min(best_enc or float("inf"), t1 - t0)
            best_dec = min(best_dec or float("inf"), t2 - t1)

        ratio = len(data) / len(compressed)
        saved = 1 - len(compressed) / len(data)
        enc_mbs = len(data) / best_enc / mib
        dec_mbs = len(data) / best_dec / mib
        row = (
            f"{level:>5} | {human(len(compressed)):>10} | {ratio:5.2f}x | "
            f"{saved * 100:6.1f}% | {enc_mbs:9.1f} | {dec_mbs:9.1f}"
        )
        if mismatch:
            row += "   !! ROUND-TRIP MISMATCH - output does not match input"
        print(row)


def main() -> None:
    args = sys.argv[1:]

    if args:
        datasets = [(pathlib.Path(p).name, pathlib.Path(p).read_bytes()) for p in args]
    else:
        rust_root = pathlib.Path(__file__).resolve().parent.parent
        datasets = real_datasets(rust_root)

    for name, data in datasets:
        benchmark(name, data)


if __name__ == "__main__":
    main()
