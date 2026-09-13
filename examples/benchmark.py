#!/usr/bin/env python3
"""Benchmark the `kanzi` extension module: density (ratio) and speed per level.

Usage:
    python examples/benchmark.py                 # use real files from this repo
    python examples/benchmark.py file1 file2...   # benchmark specific files
    python examples/benchmark.py --strict         # exit(1) on any round-trip mismatch
    python examples/benchmark.py --repeats N file # keep the fastest of N runs (default 3)

With no arguments, benchmarks real, representative content already sitting in
this repo instead of made-up data:
  - a random-bytes baseline (the incompressible floor every codec must respect)
  - this project's own README (real English prose)
  - this project's own Rust source (real, mixed-content source code)
  - verify/kanzi.exe and a built wheel, if present locally (dev-only bonus --
    both are gitignored, so a fresh clone/CI checkout won't have them)
Any of these that isn't present on disk is skipped rather than faked. `--strict`
is what CI uses: same benchmark, but a mismatch fails the job instead of just
being flagged in the output -- this is the check that would have caught the
RLT run-length bug fixed in this repo's history.
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
    """Collect real files already present in the repo, skipping what's missing.

    Everything referenced here lives inside this repo (rust_root itself), so
    this works the same whether it's run from a full clone, a fresh CI
    checkout, or (as in earlier development) nested inside a larger
    monorepo checkout -- unlike referencing sibling-repo paths, which
    silently stop resolving the moment this repo is cloned on its own.
    """
    datasets: list[tuple[str, bytes]] = [
        ("random bytes (incompressible baseline)", os.urandom(2 * 1024 * 1024))
    ]

    readme = rust_root / "README.md"
    if readme.exists():
        datasets.append(("project README (real English prose)", readme.read_bytes()))

    rs_files = sorted(rust_root.glob("src/*.rs"))
    if rs_files:
        blob = b"".join(p.read_bytes() for p in rs_files)
        datasets.append((f"this project's Rust source ({len(rs_files)} real files)", blob))

    # Dev-only bonus datasets: gitignored, so only present if built locally.
    exe_path = rust_root / "verify" / "kanzi.exe"
    if exe_path.exists():
        datasets.append(("native binary (kanzi.exe, real machine code)", exe_path.read_bytes()))

    wheels = sorted((rust_root / "target" / "wheels").glob("*.whl"))
    if wheels:
        datasets.append((f"compressed archive ({wheels[-1].name}, real zip)", wheels[-1].read_bytes()))

    return datasets


def benchmark(name: str, data: bytes, repeats: int = REPEATS) -> bool:
    """Runs the benchmark for one dataset; returns True iff every level's
    round trip matched the input."""
    print(f"\n=== {name}  ({len(data):,} bytes) ===")
    header = f"{'level':>5} | {'bytes':>12} | {'size':>10} | {'ratio':>6} | {'saved':>7} | {'enc MB/s':>9} | {'dec MB/s':>9}"
    print(header)
    print("-" * len(header))

    all_ok = True
    mib = 1024 * 1024
    for level in LEVELS:
        best_enc = best_dec = None
        compressed = b""
        mismatch = False
        for _ in range(repeats):
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
            f"{level:>5} | {len(compressed):>12,} | {human(len(compressed)):>10} | {ratio:5.2f}x | "
            f"{saved * 100:6.1f}% | {enc_mbs:9.1f} | {dec_mbs:9.1f}"
        )
        if mismatch:
            row += "   !! ROUND-TRIP MISMATCH - output does not match input"
            all_ok = False
        print(row)

    return all_ok


def main() -> None:
    args = sys.argv[1:]
    strict = "--strict" in args
    args = [a for a in args if a != "--strict"]

    repeats = REPEATS
    if "--repeats" in args:
        i = args.index("--repeats")
        repeats = int(args[i + 1])
        args = args[:i] + args[i + 2 :]

    if args:
        datasets = [(pathlib.Path(p).name, pathlib.Path(p).read_bytes()) for p in args]
    else:
        rust_root = pathlib.Path(__file__).resolve().parent.parent
        datasets = real_datasets(rust_root)

    all_ok = True
    for name, data in datasets:
        all_ok &= benchmark(name, data, repeats)

    if strict and not all_ok:
        print("\n--strict: at least one round-trip mismatch above, failing.", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
