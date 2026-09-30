//! Python bindings for virtx, imported as `virtx._virtx`.
//!
//! The shape is virtx's own, spelled in Python: a [`ConsoleClient`](virtx::console::ConsoleClient)
//! is built by a builder and awaited, a [`Directory`](virtx::fs::Directory) is assembled and
//! handed to a host mount, a [`Recipe`](virtx::image::Recipe) is a base and its steps, and an
//! [`ImageClient`](virtx::image::ImageClient) builds one ahead of the session that runs on it.
//! Nothing here adds a layer of its own over that — a Python caller reading virtx's Rust
//! documentation should find the same names doing the same things.
//!
//! One module per half, as in the crate: [`console`] for running commands, [`fs`] for the
//! trees they see, [`image`] for what they run on and the client that builds it, and [`error`] for how either half's
//! failures arrive as exceptions.

use pyo3::prelude::*;

pub mod console;
#[cfg(feature = "ensure")]
pub mod ensure;
pub mod error;
pub mod fs;
pub mod image;

#[pymodule]
fn _virtx(m: &Bound<'_, PyModule>) -> PyResult<()> {
    register(m)
}

/// Add every class, exception and constant this module has to `m`.
///
/// The module's own init, and also what a binding that links this crate calls to put virtx
/// into an extension of its own — see the `rlib` in `Cargo.toml`.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    error::register(m)?;
    image::register(m)?;
    fs::register(m)?;
    console::register(m)?;
    #[cfg(feature = "ensure")]
    ensure::register(m)?;
    Ok(())
}
