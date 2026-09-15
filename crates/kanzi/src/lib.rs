//! A Rust port of [Kanzi](https://github.com/flanglet/kanzi-go), Frederic
//! Langlet's lossless data compressor. Streams are bitstream format v7 and
//! byte-compatible with kanzi-go and kanzi-cpp at every level 0-9.
//!
//! [`compress`] and [`decompress`] cover the whole container. The individual
//! transforms and entropy coders are public too, so tools can drive them
//! directly (the `kanzi-cli` crate's cross-check subcommands do).

pub mod alias;
pub mod ans;
pub mod binary_entropy;
pub mod bitio;
pub mod bwt;
mod cm;
pub mod container;
pub mod datatype;
// Used by bwt.rs's build_suffix_array only when the `fast-sa` feature is
// off (the default build uses libsais instead, see bwt.rs) -- allow
// dead_code outside tests so the default build stays warning-free without
// gating every item in the module behind the feature flag individually.
// `compute_bwt` specifically is also never called in *either* build (this
// crate's bwt.rs builds BWT bytes itself from the plain suffix array
// rather than DivSufSort's fused constructBWT); it stays public and
// tested as a faithful, usable port of the original API surface.
#[cfg_attr(not(test), allow(dead_code))]
mod divsufsort;
pub mod exe;
pub mod fpaq;
pub mod fsd;
pub mod huffman_dec;
pub mod huffman_enc;
mod logtables;
mod lzp;
pub mod lzx;
mod magic;
pub mod rlt;
pub mod rolz;
// Kept only as an independent correctness oracle for divsufsort.rs's tests
// and the CLI's `saistest` -- bwt.rs no longer calls it.
#[doc(hidden)]
pub mod sais;
pub mod sbrt;
pub mod srt;
pub mod text_codec;
pub mod text_codec1;
pub mod tpaq;
pub mod utf;
pub mod xxhash;
pub mod zrlt;

pub const DEFAULT_BLOCK_SIZE: u32 = 4 * 1024 * 1024;

/// Per-level default block size, matching kanzi-go's BlockCompressor exactly
/// (app/BlockCompressor.go): higher levels use bigger blocks so their
/// adaptive entropy models (FPAQ/CM/TPAQ/TPAQX) get more data to learn from
/// per reset instead of restarting every 4 MiB. Using a flat 4 MiB for every
/// level cost 1.5-2.8% compression ratio at levels 6-9 on real text -- not
/// an algorithmic bug, just models reset far more often than the reference
/// implementation resets them.
pub fn default_block_size(level: u32) -> u32 {
    match level {
        6 => 2 * DEFAULT_BLOCK_SIZE,
        7 | 8 => 4 * DEFAULT_BLOCK_SIZE,
        9 => 8 * DEFAULT_BLOCK_SIZE,
        _ => DEFAULT_BLOCK_SIZE,
    }
}

/// Compresses `data` at `level` (0-9) into a complete Kanzi container.
/// `block_size` overrides the level's default (see [`default_block_size`]),
/// matching the reference CLI's `-b`/`--block`.
pub fn compress(data: &[u8], level: u32, block_size: Option<u32>) -> Result<Vec<u8>, String> {
    let block_size = block_size.unwrap_or_else(|| default_block_size(level));

    let out = match level {
        0 => container::encode_level0(data, block_size, 0),
        1 => container::encode_level1(data, block_size, 0),
        2 => container::encode_level2(data, block_size, 0),
        3 => container::encode_level3(data, block_size, 0),
        4 => container::encode_level4(data, block_size, 0),
        5 => container::encode_level5(data, block_size, 0),
        6 => container::encode_level6(data, block_size, 0),
        7 => container::encode_level7(data, block_size, 0),
        8 => container::encode_level8(data, block_size, 0),
        9 => container::encode_level9(data, block_size, 0),
        _ => return Err(format!("level must be 0-9, got {level}")),
    };

    Ok(out)
}

/// Decompresses a complete Kanzi container held in memory.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    container::decode(data)
}

/// Decompresses a Kanzi container from `input` into `out`, streaming both
/// ways: memory stays bounded by the stream's block size (a few blocks at a
/// time), not by the input or output size. Returns the number of bytes
/// written. On error, output for the blocks before the failing one has
/// already been written.
pub fn decompress_to<R: std::io::Read, W: std::io::Write>(input: R, out: &mut W) -> Result<u64, String> {
    container::decode_to(input, out)
}
