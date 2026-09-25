//! `Directory` and `HostMount`: the trees a session's commands see.
//!
//! A `Directory` is assembled in place, unlike an `Image`, because the Rust type is not
//! `Clone` — its files live in memory and a copy would be a second tree rather than a second
//! handle on one. Mounting it *takes* it: a `HostMount` owns the tree it serves, and the
//! `Directory` it was built from is empty afterwards and refuses further use.
//!
//! `HostMount` is one name for three types. cortex names each binding's guard after the
//! binding — `FuseMount`, `FuseTMount`, `DokanMount` — and only the one this platform has is
//! compiled, so a Python caller who wants "mount this on the host" should not have to know
//! which it is.

#[cfg(feature = "mount")]
use std::sync::Arc;
use std::{io, path::PathBuf};

use cortex::fs::{Directory, Mount};
use pyo3::{exceptions::PyValueError, prelude::*};

#[cfg(all(feature = "mount", windows))]
use cortex::fs::DokanMount as Platform;
#[cfg(all(feature = "mount", unix, not(target_os = "macos")))]
use cortex::fs::FuseMount as Platform;
#[cfg(all(feature = "mount", target_os = "macos"))]
use cortex::fs::FuseTMount as Platform;

/// File content as a caller may spell it: bytes as they are, a string as its UTF-8.
#[derive(FromPyObject)]
pub enum Content {
    Bytes(Vec<u8>),
    Text(String),
}

impl From<Content> for Vec<u8> {
    fn from(content: Content) -> Vec<u8> {
        match content {
            Content::Bytes(bytes) => bytes,
            Content::Text(text) => text.into_bytes(),
        }
    }
}

#[pyclass(name = "Directory", module = "cortex")]
pub struct PyDirectory(Option<Directory>);

impl PyDirectory {
    fn get(&mut self) -> PyResult<&mut Directory> {
        self.0.as_mut().ok_or_else(taken)
    }

    #[cfg_attr(not(feature = "mount"), allow(dead_code))]
    fn take(&mut self) -> PyResult<Directory> {
        self.0.take().ok_or_else(taken)
    }
}

fn taken() -> PyErr {
    PyValueError::new_err("this Directory has been mounted, and the mount owns it now")
}

#[pymethods]
impl PyDirectory {
    #[new]
    fn new() -> Self {
        PyDirectory(Some(Directory::new()))
    }

    fn add_file(&mut self, path: PathBuf, content: Content) -> PyResult<()> {
        let content = Vec::<u8>::from(content);
        Ok(self.get()?.add_file(path, io::Cursor::new(content))?)
    }

    fn remove_file(&mut self, path: PathBuf) -> PyResult<()> {
        Ok(self.get()?.remove_file(path)?)
    }

    fn mount(&mut self, path: PathBuf, host_dir: PathBuf) -> PyResult<()> {
        Ok(self.get()?.mount(path, host_dir)?)
    }

    fn unmount(&mut self, path: PathBuf) -> PyResult<()> {
        Ok(self.get()?.unmount(path)?)
    }

    /// [`add_file`](Self::add_file), handing back this same `Directory` so calls chain.
    fn with_file(
        mut slf: PyRefMut<'_, Self>,
        path: PathBuf,
        content: Content,
    ) -> PyResult<PyRefMut<'_, Self>> {
        slf.add_file(path, content)?;
        Ok(slf)
    }

    /// [`mount`](Self::mount), handing back this same `Directory` so calls chain.
    fn with_mount(
        mut slf: PyRefMut<'_, Self>,
        path: PathBuf,
        host_dir: PathBuf,
    ) -> PyResult<PyRefMut<'_, Self>> {
        slf.mount(path, host_dir)?;
        Ok(slf)
    }
}

/// A tree mounted on this host, for as long as something holds it.
///
/// Held behind an [`Arc`] so that passing one to a console builder does not take it from
/// the Python object: both hold the mount, and it comes down when the last of them lets go
/// — the builder's copy with the console, the Python one with garbage collection.
#[cfg(feature = "mount")]
#[pyclass(name = "HostMount", module = "cortex", frozen)]
pub struct PyHostMount(pub Arc<Platform>);

#[cfg(feature = "mount")]
#[pymethods]
impl PyHostMount {
    #[new]
    fn new(py: Python<'_>, fs: &Bound<'_, PyDirectory>, mountpoint: PathBuf) -> PyResult<Self> {
        let directory = fs.borrow_mut().take()?;
        // Mounting waits on the host's FUSE provider, which has no reason to wait on the GIL.
        let mount = py.detach(|| Platform::try_new(directory, &mountpoint))?;
        Ok(PyHostMount(Arc::new(mount)))
    }

    #[getter]
    fn mountpoint(&self) -> PathBuf {
        self.0.mountpoint().to_path_buf()
    }

    fn __repr__(&self) -> String {
        format!("HostMount({:?})", self.0.mountpoint())
    }
}

/// What a console builder mounts: a `HostMount`, or a host directory by its path.
#[derive(FromPyObject)]
pub enum MountLike {
    #[cfg(feature = "mount")]
    Host(Py<PyHostMount>),
    Path(PathBuf),
}

impl MountLike {
    pub fn into_mount(self) -> PyResult<Box<dyn Mount>> {
        Ok(match self {
            #[cfg(feature = "mount")]
            MountLike::Host(mount) => Box::new(mount.get().0.clone()),
            // Absolute, because a mount is named to the server as a `file://` URL, and a
            // relative path from Python is relative to where the interpreter stands.
            MountLike::Path(path) => Box::new(std::path::absolute(path)?),
        })
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDirectory>()?;
    #[cfg(feature = "mount")]
    m.add_class::<PyHostMount>()?;
    Ok(())
}
