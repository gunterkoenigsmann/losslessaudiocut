//! Linker arguments for a Python extension module.
//!
//! An extension module does not link libpython; the interpreter that
//! imports it provides the symbols. maturin arranges that for the wheels,
//! and `.cargo/config.toml` sets `PYO3_BUILD_EXTENSION_MODULE` for plain
//! `cargo build`/`clippy` runs. On macOS the linker must additionally be
//! told to leave the Python symbols undefined (`-undefined
//! dynamic_lookup`), which this adds; elsewhere it does nothing.

fn main() {
    pyo3_build_config::add_extension_module_link_args();
}
