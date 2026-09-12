mod alias;
mod ans;
mod binary_entropy;
mod bitio;
mod bwt;
mod cm;
mod container;
mod datatype;
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

/// Compress `data` with the given *level* (0‑9) and return the Kanzi container.
#[pyfunction]
fn compress(data: Vec<u8>, level: i32) -> PyResult<Vec<u8>> {
    let out = match level {
        0 => crate::container::encode_level0(&data, 4_194_304, 0),
        1 => crate::container::encode_level1(&data, 4_194_304, 0),
        2 => crate::container::encode_level2(&data, 4_194_304, 0),
        3 => crate::container::encode_level3(&data, 4_194_304, 0),
        4 => crate::container::encode_level4(&data, 4_194_304, 0),
        5 => crate::container::encode_level5(&data, 4_194_304, 0),
        6 => crate::container::encode_level6(&data, 4_194_304, 0),
        7 => crate::container::encode_level7(&data, 4_194_304, 0),
        8 => crate::container::encode_level8(&data, 4_194_304, 0),
        9 => crate::container::encode_level9(&data, 4_194_304, 0),
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
    let compressed = compress(data.clone(), level)?;
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