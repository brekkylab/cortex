//! Python bindings for cortex, imported as `cortex._cortex`.
//!
//! The shape is cortex's own, spelled in Python: a [`ConsoleClient`](cortex::console::ConsoleClient)
//! is built by a builder and awaited, a [`Directory`](cortex::fs::Directory) is assembled and
//! handed to a host mount, a [`Recipe`](cortex::image::Recipe) is a base and its steps, and an
//! [`ImageClient`](cortex::image::ImageClient) builds one ahead of the session that runs on it.
//! Nothing here adds a layer of its own over that — a Python caller reading cortex's Rust
//! documentation should find the same names doing the same things.
//!
//! One module per half, as in the crate: [`console`] for running commands, [`fs`] for the
//! trees they see, [`image`] for what they run on and the client that builds it, and [`error`] for how either half's
//! failures arrive as exceptions.

use pyo3::prelude::*;

pub mod console;
pub mod error;
pub mod fs;
pub mod image;

#[pymodule]
fn _cortex(m: &Bound<'_, PyModule>) -> PyResult<()> {
    register(m)
}

/// Add every class, exception and constant this module has to `m`.
///
/// The module's own init, and also what a binding that links this crate calls to put cortex
/// into an extension of its own — see the `rlib` in `Cargo.toml`.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    error::register(m)?;
    image::register(m)?;
    fs::register(m)?;
    console::register(m)?;
    Ok(())
}
