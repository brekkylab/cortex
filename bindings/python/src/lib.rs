//! Python bindings for cortex, imported as `cortex._cortex`.
//!
//! The shape is cortex's own, spelled in Python: a [`Console`](cortex::console::Console) is
//! built by a builder and awaited, a [`Directory`](cortex::fs::Directory) is assembled and
//! handed to a host mount, and an [`Image`](cortex::image::Image) is a base and its steps.
//! Nothing here adds a layer of its own over that — a Python caller reading cortex's Rust
//! documentation should find the same names doing the same things.
//!
//! One module per half, as in the crate: [`console`] for running commands, [`fs`] for the
//! trees they see, [`image`] for what they run on, and [`error`] for how either half's
//! failures arrive as exceptions.

use pyo3::prelude::*;

mod console;
mod error;
mod fs;
mod image;

#[pymodule]
fn _cortex(m: &Bound<'_, PyModule>) -> PyResult<()> {
    error::register(m)?;
    image::register(m)?;
    fs::register(m)?;
    console::register(m)?;
    Ok(())
}
