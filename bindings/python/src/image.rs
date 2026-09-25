//! `Image` and `Step`: what a session's commands run on.
//!
//! Both are values, as they are in Rust — `Image` is `Clone` and a builder call there hands
//! back a new one — so here every method returns a new object and none mutates the one it
//! was called on. A base image built once and extended in two directions stays the base.

use std::path::PathBuf;

use cortex::image::{Image, Step};
use pyo3::prelude::*;

use crate::error;

#[pyclass(name = "Step", module = "cortex", frozen, from_py_object)]
#[derive(Clone)]
pub struct PyStep(pub Step);

#[pymethods]
impl PyStep {
    #[staticmethod]
    fn run(cmd: String) -> Self {
        PyStep(Step::run(cmd))
    }

    #[staticmethod]
    fn copy(src: PathBuf, dst: String) -> Self {
        PyStep(Step::copy(src, dst))
    }

    #[staticmethod]
    fn env(key: String, value: String) -> Self {
        PyStep(Step::env(key, value))
    }

    #[staticmethod]
    fn workdir(dir: String) -> Self {
        PyStep(Step::workdir(dir))
    }

    fn __repr__(&self) -> String {
        format!("Step({})", self.0)
    }
}

/// A step as a caller may spell one: a `Step`, or a string meaning `Step.run(..)` — the
/// same conversion `From<&str> for Step` makes in Rust.
#[derive(FromPyObject)]
pub enum StepLike {
    Step(PyStep),
    Run(String),
}

impl From<StepLike> for Step {
    fn from(step: StepLike) -> Step {
        match step {
            StepLike::Step(step) => step.0,
            StepLike::Run(cmd) => Step::run(cmd),
        }
    }
}

#[pyclass(name = "Image", module = "cortex", frozen, from_py_object)]
#[derive(Clone)]
pub struct PyImage(pub Image);

#[pymethods]
impl PyImage {
    #[new]
    #[pyo3(signature = (base = None, steps = Vec::new()))]
    fn new(base: Option<String>, steps: Vec<StepLike>) -> Self {
        let mut image = Image::new().steps(steps);
        if let Some(base) = base {
            image = image.base(base);
        }
        PyImage(image)
    }

    #[staticmethod]
    fn from_dockerfile(content: &str) -> PyResult<Self> {
        Image::from_dockerfile(content)
            .map(PyImage)
            .map_err(error::anyhow)
    }

    fn base(&self, base: String) -> Self {
        PyImage(self.0.clone().base(base))
    }

    fn step(&self, step: StepLike) -> Self {
        PyImage(self.0.clone().step(step))
    }

    fn steps(&self, steps: Vec<StepLike>) -> Self {
        PyImage(self.0.clone().steps(steps))
    }

    fn __repr__(&self) -> String {
        let steps: Vec<String> = self.0.steps.iter().map(|s| format!("{s}")).collect();
        format!("Image(base={:?}, steps={steps:?})", self.0.base)
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyStep>()?;
    m.add_class::<PyImage>()?;
    Ok(())
}
