//! `ensure_cortex`: fetch the console server when this host has none.
//!
//! Console builders and `ImageClient` start `cortex-krun` from cortex's cache, which a host that
//! installed only this package lacks, so such a host calls this first.

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;

use crate::error;

/// Fetch the console server into cortex's cache if missing; the awaitable resolves to its
/// directory.
///
/// An existing server, fetched or installed by hand, is left alone.
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
