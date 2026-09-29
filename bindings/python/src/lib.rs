//! Python bindings for cortex, imported as `cortex._cortex`.
//!
//! The API is cortex's own with no extra layer, so cortex's Rust docs apply: a
//! [`ConsoleClient`](cortex::console::ConsoleClient) is built by a builder and awaited, a
//! [`Directory`](cortex::fs::Directory) is assembled and handed to a host mount, a
//! [`Recipe`](cortex::image::Recipe) is a base plus steps, and an
//! [`ImageClient`](cortex::image::ImageClient) builds images ahead of the sessions that run on them.
//!
//! Modules mirror the crate: [`console`] runs commands, [`fs`] builds the trees they see,
//! [`image`] covers what they run on and the client that builds it, and [`error`] maps failures
//! to exceptions.

use pyo3::prelude::*;

pub mod console;
#[cfg(feature = "ensure")]
pub mod ensure;
pub mod error;
pub mod fs;
pub mod image;

#[pymodule]
fn _cortex(m: &Bound<'_, PyModule>) -> PyResult<()> {
    register(m)
}

/// Add every class, exception and constant this module has to `m`.
///
/// Called by this module's init, and by a binding that links this crate into its own extension
/// (the `rlib` in `Cargo.toml`).
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    error::register(m)?;
    image::register(m)?;
    fs::register(m)?;
    console::register(m)?;
    #[cfg(feature = "ensure")]
    ensure::register(m)?;
    Ok(())
}
