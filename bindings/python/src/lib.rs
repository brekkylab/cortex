//! Python bindings for cortex, imported as `cortex._cortex`.
//!
//! The API is cortex's own with no extra layer, so cortex's Rust docs apply: a
//! [`ConsoleClient`](cortex::console::ConsoleClient) is built by a builder and awaited, a
//! [`Directory`](cortex::fs::Directory) is assembled and handed to a host mount, a
//! [`Recipe`](cortex::image::Recipe) is a base plus steps, and an
//! [`ImageClient`](cortex::image::ImageClient) builds images ahead of the sessions that run on them.
//!
//! Modules mirror the crate: [`console`] runs commands, [`fs`] builds the trees they see,
//! [`image`] covers what they run on and the client that builds it, [`error`] maps failures
//! to exceptions, and `ensure` fetches the console server.
//!
//! Every call that waits is an awaitable run on the tokio runtime `pyo3-async-runtimes` keeps:
//! a stdio client spawns its server and reads from it, which needs a reactor asyncio lacks.
//! `ConsoleClient` and `ImageClient` keep their Rust client in an `Arc<Mutex<Option<..>>>`.
//! Each call clones the `Arc` into its `'static` future, and the lock makes calls take turns on
//! the one channel, as `&mut self` does in Rust: a second `exec` awaited alongside the first
//! waits for it rather than interleaving with it. The Rust client's `Drop` says `quit` only on
//! a runtime and a Python finalizer runs off one, so the last holder drops it inside the
//! binding's runtime: a garbage-collected client ends the same way as a closed one, and
//! `close()` or `async with` only pick the line where it ends.

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
