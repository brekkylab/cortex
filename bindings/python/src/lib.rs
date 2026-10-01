//! Python bindings for virtx, imported as `virtx._virtx`.
//!
//! The API is virtx's own with no extra layer, so virtx's Rust docs apply. Modules mirror
//! the crate: [`console`] runs commands in a [`ConsoleClient`](virtx::console::ConsoleClient)
//! built by a builder and awaited, [`fs`] assembles a [`Directory`](virtx::fs::Directory) for
//! a host mount, [`image`] covers a [`Recipe`](virtx::image::Recipe) (a base plus steps) and
//! the [`ImageClient`](virtx::image::ImageClient) that builds it ahead of a session, [`error`]
//! maps failures to exceptions, and `ensure` fetches the console server.
//!
//! Every call that waits is an awaitable run on the tokio runtime `pyo3-async-runtimes` keeps,
//! since a stdio client spawns and reads its server, which needs a reactor asyncio lacks.
//! `ConsoleClient` and `ImageClient` keep their Rust client in an `Arc<Mutex<Option<..>>>`:
//! each call clones the `Arc` into its `'static` future, and the lock makes calls take turns on
//! the one channel, as `&mut self` does in Rust. The Rust client's `Drop` says `quit` only on a
//! runtime, and a Python finalizer runs off one, so the last holder drops it inside the
//! binding's runtime: a garbage-collected client ends like a closed one, and `close()` or
//! `async with` only pick when.

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

/// Add every class, exception and constant to `m`; public so a binding linking this crate (the
/// `rlib` in `Cargo.toml`) can add them to its own extension.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    error::register(m)?;
    image::register(m)?;
    fs::register(m)?;
    console::register(m)?;
    #[cfg(feature = "ensure")]
    ensure::register(m)?;
    Ok(())
}
