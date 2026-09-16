fn main() {
    // PyO3's `extension-module` feature deliberately doesn't link against
    // libpython: the symbols resolve when the interpreter loads the module.
    // ld64 on macOS rejects undefined symbols in a dylib by default, so a
    // plain `cargo build -p kanzi-py` there needs `-undefined dynamic_lookup`
    // (maturin passes it on its own). This is PyO3's documented fix, and
    // living here instead of a workspace-wide .cargo/config.toml keeps the
    // flag off the pure-Rust crates.
    pyo3_build_config::add_extension_module_link_args();
}
