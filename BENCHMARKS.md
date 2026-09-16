# Benchmarks

kanzi-rs vs the reference implementations (kanzi-go v2.5.1, kanzi-cpp),
same corpus, same machine, native CLI binaries only -- no Python in the
loop, since the binding's buffer-marshaling cost is fixed per call and
swamps real work at low levels (a few hundred ms fixed cost vs a few
hundred ms of real work at level 1, vs twenty seconds at level 9).

For the detailed investigation history behind any of these numbers
(what was tried, what measured as a real win, what didn't and why) see
git log -- this file now stays a clean benchmark record, not a running
diary.

## silesia.tar

Download: http://sun.aei.polsl.pl/~sdeor/corpus/silesia.zip -- tar the
12 extracted files together as `silesia.tar` (211,957,760 bytes).

Command: `-c`/`-d -l N -j 0` (all available cores), each project's own
per-level default block size (kanzi-rs's CLI mirrors this automatically
when no explicit block size is given).

### AMD Ryzen 9 5950X (16C/32T, 4000 MHz all-core, Windows 10, rustc 1.98.1)

kanzi-cpp built from `msvc/Kanzi_VS2022.sln`, MSVC 14.44/Release/x64.

Measured at `a86b328`, before the streaming and decode work that went into
0.2.0, and not re-run since -- this machine is no longer available. Kept as
a record of how the three implementations compared at that point; the
i3-12100 table below is the current one.

| Level | kanzi-rs enc/dec (ms) | kanzi-go enc/dec (ms) | kanzi-cpp enc/dec (ms) | kanzi-rs size | kanzi-go size | kanzi-cpp size |
|---|---|---|---|---|---|---|
| 1 | 419 / 218 | 319 / 191 | 219 / 118 | 79,202,777 | 79,202,781 | 79,202,781 |
| 2 | 349 / 254 | 259 / 208 | 204 / 126 | 68,646,055 | 68,646,059 | 68,646,059 |
| 3 | 471 / 265 | 411 / 238 | 274 / 145 | 64,451,851 | 64,436,766 | 64,436,766 |
| 4 | 721 / 420 | 771 / 354 | 386 / 216 | 61,192,921 | 61,192,925 | 60,738,928 |
| 5 | 2761 / 616 | 1480 / 665 | 1246 / 507 | 54,021,324 | 54,021,328 | 54,021,328 |
| 6 | 3799 / 744 | 1836 / 866 | 1774 / 944 | 49,515,942 | 49,515,946 | 49,515,946 |
| 7 | 4154 / 1785 | 2725 / 4126 | 2675 / 4596 | 47,309,589 | 47,309,593 | 47,309,593 |
| 8 | 8523 / 8702 | 11843 / 12057 | 10149 / 7970 | 43,257,955 | 43,257,959 | 43,261,199 |
| 9 | 20257 / 20812 | 22487 / 27791 | 25308 / 22860 | 41,857,565 | 41,857,569 | 41,857,569 |

Sizes match at least one reference within a handful of bytes at every
level (kanzi-cpp's own numbers diverge slightly from both Go and Rust at
levels 3/4/8 -- "the reference" isn't perfectly bit-identical across
kanzi-go and kanzi-cpp either). Speed: kanzi-cpp fastest at levels 1-6,
kanzi-rs 1.1-2.6x slower there and roughly in line with kanzi-go; at
levels 8-9 kanzi-rs is fastest of the three on both encode and decode
(32-thread block-level parallelism paying off specifically where the
adaptive entropy coders make it matter most).

### Intel i3-12100 (4C/8T, 3.3 GHz, 16 GB RAM, Windows 11, rustc 1.98.1)

kanzi-cpp 2.5.3 and kanzi-go 2.5.1. kanzi-cpp built with MSYS2 GCC 16.1
(`-O3 -march=native`) -- MSVC's `Kanzi_VS2022.vcxproj` exe project fails to
build here (broken relative source paths; only the lib project builds via
MSBuild on this checkout).

Wall-clock of the whole process for all three, so process startup is
included on equal terms; best of 3 at levels 1-7 and best of 2 at levels
8-9. Every decode was checked against the source SHA-256.

| Level | kanzi-rs enc/dec (ms) | kanzi-go enc/dec (ms) | kanzi-cpp enc/dec (ms) | kanzi-rs size | kanzi-go size | kanzi-cpp size |
|---|---|---|---|---|---|---|
| 1 | 418 / 108 | 635 / 218 | 417 / 107 | 79,204,076 | 79,344,170 | 79,204,080 |
| 2 | 260 / 122 | 461 / 246 | 240 / 120 | 68,649,853 | 68,633,054 | 68,649,857 |
| 3 | 621 / 172 | 916 / 347 | 552 / 171 | 64,392,664 | 64,582,577 | 64,377,553 |
| 4 | 1,063 / 313 | 1,569 / 578 | 846 / 315 | 60,710,689 | 61,264,293 | 60,710,693 |
| 5 | 2,016 / 1,014 | 3,316 / 1,460 | 2,716 / 1,066 | 54,025,363 | 54,016,012 | 54,025,367 |
| 6 | 3,119 / 1,287 | 4,961 / 2,628 | 3,451 / 1,327 | 49,517,362 | 49,517,340 | 49,517,366 |
| 7 | 4,299 / 2,378 | 6,766 / 4,967 | 4,863 / 3,349 | 47,308,669 | 47,308,660 | 47,308,673 |
| 8 | 16,426 / 16,281 | 21,623 / 22,482 | 15,610 / 17,052 | 43,260,495 | 43,257,270 | 43,260,499 |
| 9 | 26,402 / 26,125 | 33,700 / 33,162 | 33,077 / 25,509 | 41,855,869 | 41,854,526 | 41,855,873 |

**Decode** is at parity with kanzi-cpp at levels 1-3 (within 1-2%), faster
at levels 4-8, and 2% behind at level 9. **Encode** is at parity at level 1,
3-26% behind at levels 2-4 -- level 4 is the widest remaining gap -- and
faster from level 5 up except level 8. kanzi-rs is faster than kanzi-go at
every level on both operations.

**Sizes** match kanzi-cpp within four bytes at every level except 3, where
kanzi-rs lands 15,111 bytes (0.023%) above it and 189,913 below kanzi-go:
a transform decision that differs three ways between the implementations,
not a correctness problem. kanzi-go's level-1 size differs from the other
two by ~0.18% because this `silesia.tar` was rebuilt locally
(permissions/timestamps baked into the tar bytes); the 12 underlying files
are the same.

Reproduce: `cargo build --release`, then time `rust_kanzi.exe
encodeN`/`decode` against `kanzi.exe -c/-d -l N -j 0` and kanzi-cpp's own
CLI built the same way.
