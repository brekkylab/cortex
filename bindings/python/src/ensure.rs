//! `ensure_cortex`: the console server, fetched when this host has none.
//!
//! What a console builder and an `ImageClient` start is `cortex-krun` under cortex's cache,
//! and a host that installed only this package has none there -- so this is the one call
//! such a host makes before anything else.

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;

use crate::error;

/// Fetch the console server into cortex's cache if it is not there, and answer the
/// directory it is in -- an awaitable, as the Rust `ensure_cortex` is a future.
///
/// A server already there is left alone, whether it was fetched or installed by hand.
#[pyfunction]
fn ensure_cortex(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    future_into_py(py, async move {
        let bin = cortex::ensure_cortex().await.map_err(error::anyhow)?;
        Ok(bin.to_string_lossy().into_owned())
    })
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(ensure_cortex, m)?)?;
    Ok(())
}
