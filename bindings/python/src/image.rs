//! `Recipe`, `Step` and `ImageSource` (what a session's commands run on), and `ImageClient`,
//! which builds, lists and removes images.
//!
//! The first three are values, as in Rust (`Recipe` is `Clone` and its builder calls return a
//! new one): every method returns a new object and none mutates its receiver, so a base recipe
//! extended in two directions stays the base.
//!
//! `ImageClient` sits in an `Arc<Mutex<Option<..>>>`: each call clones it into a `'static`
//! future on the binding's runtime, the lock makes calls take turns on the one channel, and it
//! is dropped inside that runtime so `quit` goes out even from a Python finalizer.

use std::{path::PathBuf, sync::Arc};

use cortex::{
    image::{ImageClient, ImageEntry, ImageSource, Recipe, Step},
    protocol::BuildImageResp,
};
use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::{future_into_py, get_runtime};
use tokio::sync::Mutex;

use crate::error::{self, CortexError};

#[pyclass(name = "Step", module = "cortex", frozen, eq, from_py_object)]
#[derive(Clone, PartialEq)]
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

    fn __str__(&self) -> String {
        self.0.to_string()
    }

    fn __repr__(&self) -> String {
        format!("Step({})", self.0)
    }
}

/// A `Step`, or a string meaning `Step.run(..)`, as `From<&str> for Step` converts in Rust.
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

#[pyclass(name = "Recipe", module = "cortex", frozen, eq, from_py_object)]
#[derive(Clone, PartialEq)]
pub struct PyRecipe(pub Recipe);

#[pymethods]
impl PyRecipe {
    #[new]
    #[pyo3(signature = (base, steps = Vec::new()))]
    fn new(base: String, steps: Vec<StepLike>) -> Self {
        PyRecipe(Recipe::new(base).steps(steps))
    }

    #[staticmethod]
    fn from_dockerfile(content: &str) -> PyResult<Self> {
        Recipe::from_dockerfile(content)
            .map(PyRecipe)
            .map_err(error::anyhow)
    }

    #[getter]
    fn base(&self) -> &str {
        &self.0.base
    }

    fn step(&self, step: StepLike) -> Self {
        PyRecipe(self.0.clone().step(step))
    }

    fn steps(&self, steps: Vec<StepLike>) -> Self {
        PyRecipe(self.0.clone().steps(steps))
    }

    fn __repr__(&self) -> String {
        let steps: Vec<String> = self.0.steps.iter().map(|s| s.to_string()).collect();
        format!("Recipe(base={:?}, steps={steps:?})", self.0.base)
    }
}

#[pyclass(name = "ImageSource", module = "cortex", frozen, eq, from_py_object)]
#[derive(Clone, PartialEq)]
pub struct PyImageSource(pub ImageSource);

#[pymethods]
impl PyImageSource {
    #[staticmethod]
    fn reference(reference: String) -> Self {
        PyImageSource(ImageSource::reference(reference))
    }

    #[staticmethod]
    fn digest(digest: String) -> Self {
        PyImageSource(ImageSource::digest(digest))
    }

    #[staticmethod]
    fn recipe(recipe: PyRecipe) -> Self {
        PyImageSource(recipe.0.into())
    }

    fn __repr__(&self) -> String {
        match &self.0 {
            ImageSource::Recipe { recipe } => {
                format!(
                    "ImageSource.recipe({})",
                    PyRecipe(recipe.clone()).__repr__()
                )
            }
            ImageSource::Ref { reference } => format!("ImageSource.reference({reference:?})"),
            ImageSource::Digest { digest } => format!("ImageSource.digest({digest:?})"),
        }
    }
}

/// An `ImageSource`, or a `Recipe` meaning `ImageSource.recipe(..)`, as
/// `From<Recipe> for ImageSource` converts in Rust.
#[derive(FromPyObject)]
pub enum ImageSourceLike {
    Source(PyImageSource),
    Recipe(PyRecipe),
}

impl From<ImageSourceLike> for ImageSource {
    fn from(image: ImageSourceLike) -> ImageSource {
        match image {
            ImageSourceLike::Source(source) => source.0,
            ImageSourceLike::Recipe(recipe) => recipe.0.into(),
        }
    }
}

#[pyclass(name = "BuildImageResult", module = "cortex", frozen, get_all)]
pub struct PyBuildImageResult {
    reference: String,
    digest: String,
}

impl From<BuildImageResp> for PyBuildImageResult {
    fn from(resp: BuildImageResp) -> Self {
        PyBuildImageResult {
            reference: resp.reference,
            digest: resp.digest,
        }
    }
}

#[pymethods]
impl PyBuildImageResult {
    fn __repr__(&self) -> String {
        format!(
            "BuildImageResult(reference={:?}, digest={:?})",
            self.reference, self.digest
        )
    }
}

#[pyclass(name = "ImageEntry", module = "cortex", frozen, get_all)]
pub struct PyImageEntry {
    digest: String,
    refs: Vec<String>,
}

impl From<ImageEntry> for PyImageEntry {
    fn from(entry: ImageEntry) -> Self {
        PyImageEntry {
            digest: entry.digest,
            refs: entry.refs,
        }
    }
}

#[pymethods]
impl PyImageEntry {
    fn __repr__(&self) -> String {
        format!("ImageEntry(digest={:?}, refs={:?})", self.digest, self.refs)
    }
}

/// The client a slot holds, or the error for one that has been closed.
fn held(slot: &mut Option<ImageClient>) -> PyResult<&mut ImageClient> {
    slot.as_mut()
        .ok_or_else(|| CortexError::new_err("this image client has been closed"))
}

#[pyclass(name = "ImageClient", module = "cortex", frozen)]
pub struct PyImageClient(Arc<Mutex<Option<ImageClient>>>);

impl PyImageClient {
    fn new(client: ImageClient) -> Self {
        PyImageClient(Arc::new(Mutex::new(Some(client))))
    }
}

/// Only the last holder drops the client, inside the runtime so `quit` goes out.
impl Drop for PyImageClient {
    fn drop(&mut self) {
        if let Some(slot) = Arc::get_mut(&mut self.0) {
            let _entered = get_runtime().enter();
            slot.get_mut().take();
        }
    }
}

#[pymethods]
impl PyImageClient {
    /// Start `cortex-krun` from cortex's cache `bin`; the awaitable resolves to the client.
    #[staticmethod]
    fn try_new(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
        future_into_py(py, async move {
            let client = ImageClient::try_new().await.map_err(error::failure)?;
            Ok(PyImageClient::new(client))
        })
    }

    #[staticmethod]
    fn try_from_cmd(py: Python<'_>, cmd: Vec<String>) -> PyResult<Bound<'_, PyAny>> {
        future_into_py(py, async move {
            let client = ImageClient::try_from_cmd(&cmd)
                .await
                .map_err(error::failure)?;
            Ok(PyImageClient::new(client))
        })
    }

    fn version<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.0.clone();
        future_into_py(py, async move {
            let mut slot = client.lock().await;
            held(&mut slot)?.version().await.map_err(error::failure)
        })
    }

    #[pyo3(signature = (recipe, reference = None))]
    fn build<'py>(
        &self,
        py: Python<'py>,
        recipe: PyRecipe,
        reference: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.0.clone();
        future_into_py(py, async move {
            let mut slot = client.lock().await;
            let resp = held(&mut slot)?.build(recipe.0, reference.as_deref()).await;
            resp.map(PyBuildImageResult::from).map_err(error::failure)
        })
    }

    fn list<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.0.clone();
        future_into_py(py, async move {
            let mut slot = client.lock().await;
            let resp = held(&mut slot)?.list().await;
            resp.map(|images| {
                images
                    .into_iter()
                    .map(PyImageEntry::from)
                    .collect::<Vec<_>>()
            })
            .map_err(error::failure)
        })
    }

    fn remove<'py>(&self, py: Python<'py>, image: ImageSourceLike) -> PyResult<Bound<'py, PyAny>> {
        let client = self.0.clone();
        let image = ImageSource::from(image);
        future_into_py(py, async move {
            let mut slot = client.lock().await;
            held(&mut slot)?.remove(image).await.map_err(error::failure)
        })
    }

    /// End the channel now. Closing twice is the same as closing once.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.0.clone();
        future_into_py(py, async move {
            // Dropping on the runtime is what lets `quit` go out.
            client.lock().await.take();
            Ok(())
        })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let slf = slf.unbind();
        future_into_py(py, async move { Ok(slf) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Bound<'py, PyAny>,
        _exc: Bound<'py, PyAny>,
        _tb: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyStep>()?;
    m.add_class::<PyRecipe>()?;
    m.add_class::<PyImageSource>()?;
    m.add_class::<PyBuildImageResult>()?;
    m.add_class::<PyImageEntry>()?;
    m.add_class::<PyImageClient>()?;
    Ok(())
}
