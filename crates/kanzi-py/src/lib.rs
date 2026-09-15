//! Python bindings for the `kanzi` crate, built by maturin as the `kanzi`
//! extension module. Everything codec-related lives in `kanzi` itself; this
//! crate only marshals bytes across the Python boundary.

use pyo3::exceptions::{PyIOError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedBytes;
use pyo3::types::PyBytes;

/// Validates a Python-side level before it reaches the codec, so negative
/// and out-of-range values raise `ValueError` rather than a runtime error.
fn checked_level(level: i32) -> PyResult<u32> {
    u32::try_from(level)
        .ok()
        .filter(|&l| l <= 9)
        .ok_or_else(|| PyValueError::new_err("level must be 0-9"))
}

/// Compress `data` with the given *level* (0-9) and return the Kanzi container.
/// `block_size` overrides the level's default block size in bytes (matching
/// the CLI's `-b`/`--block`); pass `None` to use the same default the
/// reference kanzi CLI uses for that level.
///
/// Takes `PyBackedBytes` rather than `Vec<u8>`: pyo3 fills a `Vec<u8>` from a
/// `bytes` object through the generic sequence protocol, one Python int per
/// byte, which cost ~7 ns/byte (~70 ms on a 10 MB input) before any
/// compression work started.
#[pyfunction]
#[pyo3(signature = (data, level, block_size=None))]
fn compress<'py>(
    py: Python<'py>,
    data: PyBackedBytes,
    level: i32,
    block_size: Option<u32>,
) -> PyResult<Bound<'py, PyBytes>> {
    let level = checked_level(level)?;
    let out = py
        .detach(|| kanzi_core::compress(&data, level, block_size))
        .map_err(PyValueError::new_err)?;
    Ok(PyBytes::new(py, &out))
}

/// Decompress a Kanzi container (produced by `compress`) and return the original data.
///
/// `PyBackedBytes` borrows the caller's `bytes` object instead of copying it
/// into a fresh `Vec` (see `compress` for why that copy was so expensive),
/// and `detach` releases the GIL for the decode itself so other Python
/// threads keep running.
#[pyfunction]
fn decompress<'py>(py: Python<'py>, data: PyBackedBytes) -> PyResult<Bound<'py, PyBytes>> {
    let out = py
        .detach(|| kanzi_core::decompress(&data))
        .map_err(|e| PyRuntimeError::new_err(format!("decode error: {}", e)))?;
    Ok(PyBytes::new(py, &out))
}

/// Convenience: compress a file and write the .kanzi container to a new file.
#[pyfunction]
fn compress_to_file(path: &str, level: i32) -> PyResult<()> {
    let level = checked_level(level)?;
    let data = std::fs::read(path)
        .map_err(|e| PyIOError::new_err(format!("failed to read {}: {}", path, e)))?;
    let compressed = kanzi_core::compress(&data, level, None).map_err(PyValueError::new_err)?;
    let out_path = format!("{}.kanzi", path);
    std::fs::write(&out_path, &compressed)
        .map_err(|e| PyIOError::new_err(format!("failed to write {}: {}", out_path, e)))?;
    Ok(())
}

/// Decompress a .kanzi container back to the original file.
#[pyfunction]
fn decompress_to_file(path: &str) -> PyResult<()> {
    let data = std::fs::read(path)
        .map_err(|e| PyIOError::new_err(format!("failed to read {}: {}", path, e)))?;
    let decoded = kanzi_core::decompress(&data)
        .map_err(|e| PyRuntimeError::new_err(format!("decode error: {}", e)))?;
    std::fs::write(path, &decoded)
        .map_err(|e| PyIOError::new_err(format!("failed to write {}: {}", path, e)))?;
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
