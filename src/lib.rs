mod alias;
mod ans;
mod binary_entropy;
mod bitio;
mod bwt;
mod cm;
mod container;
mod datatype;
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
mod exe;
mod fpaq;
mod fsd;
mod huffman_dec;
mod huffman_enc;
mod logtables;
mod lzp;
mod lzx;
mod magic;
mod rlt;
mod rolz;
// Kept only as an independent correctness oracle for divsufsort.rs's tests
// (see divsufsort.rs's module doc and test module) -- bwt.rs no longer
// calls this in a non-test build, hence the blanket allow below.
#[cfg_attr(not(test), allow(dead_code))]
mod sais;
mod sbrt;
mod srt;
mod text_codec;
mod text_codec1;
mod tpaq;
mod utf;
mod xxhash;
mod zrlt;

use pyo3::prelude::*;

const DEFAULT_BLOCK_SIZE: u32 = 4 * 1024 * 1024;

/// Per-level default block size, matching kanzi-go's BlockCompressor exactly
/// (app/BlockCompressor.go): higher levels use bigger blocks so their
/// adaptive entropy models (FPAQ/CM/TPAQ/TPAQX) get more data to learn from
/// per reset instead of restarting every 4 MiB. This mattered a lot in
/// practice -- using a flat 4 MiB for every level (this function's previous
/// behavior) cost 1.5-2.8% compression ratio at levels 6-9 on real text,
/// not because of any algorithmic bug, just because the models were being
/// reset far more often than the reference implementation resets them.
fn default_block_size(level: i32) -> u32 {
    match level {
        6 => 2 * DEFAULT_BLOCK_SIZE,
        7 | 8 => 4 * DEFAULT_BLOCK_SIZE,
        9 => 8 * DEFAULT_BLOCK_SIZE,
        _ => DEFAULT_BLOCK_SIZE,
    }
}

/// Compress `data` with the given *level* (0‑9) and return the Kanzi container.
/// `block_size` overrides the level's default block size in bytes (matching
/// the CLI's `-b`/`--block`); pass `None` to use the same default the
/// reference kanzi CLI uses for that level.
#[pyfunction]
#[pyo3(signature = (data, level, block_size=None))]
fn compress(data: Vec<u8>, level: i32, block_size: Option<u32>) -> PyResult<Vec<u8>> {
    let block_size = block_size.unwrap_or_else(|| default_block_size(level));
    let out = match level {
        0 => crate::container::encode_level0(&data, block_size, 0),
        1 => crate::container::encode_level1(&data, block_size, 0),
        2 => crate::container::encode_level2(&data, block_size, 0),
        3 => crate::container::encode_level3(&data, block_size, 0),
        4 => crate::container::encode_level4(&data, block_size, 0),
        5 => crate::container::encode_level5(&data, block_size, 0),
        6 => crate::container::encode_level6(&data, block_size, 0),
        7 => crate::container::encode_level7(&data, block_size, 0),
        8 => crate::container::encode_level8(&data, block_size, 0),
        9 => crate::container::encode_level9(&data, block_size, 0),
        _ => return Err(pyo3::exceptions::PyValueError::new_err("level must be 0‑9")),
    };
    Ok(out)
}

/// Decompress a Kanzi container (produced by `compress`) and return the original data.
#[pyfunction]
fn decompress(data: Vec<u8>) -> PyResult<Vec<u8>> {
    crate::container::decode(&data).map_err(|e| {
        pyo3::exceptions::PyRuntimeError::new_err(format!("decode error: {}", e))
    })
}

/// Convenience: compress a file and write the .kanzi container to a new file.
#[pyfunction]
fn compress_to_file(path: &str, level: i32) -> PyResult<()> {
    let data = std::fs::read(path).map_err(|e| {
        pyo3::exceptions::PyIOError::new_err(format!("failed to read {}: {}", path, e))
    })?;
    let compressed = compress(data.clone(), level, None)?;
    let out_path = format!("{}.kanzi", path);
    std::fs::write(&out_path, &compressed).map_err(|e| {
        pyo3::exceptions::PyIOError::new_err(format!("failed to write {}: {}", out_path, e))
    })?;
    Ok(())
}

/// Decompress a .kanzi container back to the original file.
#[pyfunction]
fn decompress_to_file(path: &str) -> PyResult<()> {
    let data = std::fs::read(path).map_err(|e| {
        pyo3::exceptions::PyIOError::new_err(format!("failed to read {}: {}", path, e))
    })?;
    let decoded = decompress(data)?;
    std::fs::write(path, &decoded).map_err(|e| {
        pyo3::exceptions::PyIOError::new_err(format!("failed to write {}: {}", path, e))
    })?;
    Ok(())
}

// ---------------------------------------------------------------------
// PyO3 module entry point – Maturin will turn this into the `kanzi` Python module.
#[pymodule]
fn kanzi(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(compress, m)?)?;
    m.add_function(wrap_pyfunction!(decompress, m)?)?;
    m.add_function(wrap_pyfunction!(compress_to_file, m)?)?;
    m.add_function(wrap_pyfunction!(decompress_to_file, m)?)?;
    Ok(())
}