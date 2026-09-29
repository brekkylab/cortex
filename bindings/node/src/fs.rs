//! `Directory` and `HostMount`: the trees a session's commands see.
//!
//! A `Directory` is mutated in place because the Rust type is not `Clone`: its files live in
//! memory, so a copy would be a second tree, not a second handle on one. Mounting it *takes* it:
//! the `HostMount` owns the tree, and the `Directory` is empty afterwards and refuses further use.
//!
//! `HostMount` names whichever guard this platform compiles (`FuseMount`, `FuseTMount` or
//! `DokanMount`), so callers need not know which.

use std::{io, path::PathBuf};
#[cfg(feature = "mount")]
use std::{path::Path, sync::Arc};

use cortex::fs::{Directory, Mount};
#[cfg(feature = "mount")]
use napi::bindgen_prelude::ClassInstance;
use napi::bindgen_prelude::{Buffer, Either, This};
use napi_derive::napi;

use crate::error::{self, Result};

#[cfg(all(feature = "mount", windows))]
use cortex::fs::DokanMount as Platform;
#[cfg(all(feature = "mount", unix, not(target_os = "macos")))]
use cortex::fs::FuseMount as Platform;
#[cfg(all(feature = "mount", target_os = "macos"))]
use cortex::fs::FuseTMount as Platform;

/// File content as a caller may spell it: a `Buffer` as it is, a string as its UTF-8.
pub type Content = Either<Buffer, String>;

pub fn bytes(content: Content) -> Vec<u8> {
    match content {
        Either::A(buffer) => buffer.to_vec(),
        Either::B(text) => text.into_bytes(),
    }
}

#[napi(js_name = "Directory")]
pub struct JsDirectory(Option<Directory>);

impl JsDirectory {
    fn get(&mut self) -> Result<&mut Directory> {
        self.0.as_mut().ok_or_else(taken)
    }
}

fn taken() -> napi::Error<String> {
    error::invalid("this Directory has been mounted, and the mount owns it now")
}

#[napi]
impl JsDirectory {
    #[napi(constructor)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        JsDirectory(Some(Directory::new()))
    }

    #[napi]
    pub fn add_file(
        &mut self,
        path: String,
        #[napi(ts_arg_type = "Buffer | string")] content: Content,
    ) -> Result<()> {
        let content = io::Cursor::new(bytes(content));
        self.get()?.add_file(path, content).map_err(error::io)
    }

    #[napi]
    pub fn remove_file(&mut self, path: String) -> Result<()> {
        self.get()?.remove_file(path).map_err(error::io)
    }

    #[napi]
    pub fn mount(&mut self, path: String, host_dir: String) -> Result<()> {
        self.get()?.mount(path, host_dir).map_err(error::io)
    }

    #[napi]
    pub fn unmount(&mut self, path: String) -> Result<()> {
        self.get()?.unmount(path).map_err(error::io)
    }

    /// `addFile`, returning this same `Directory` so calls chain.
    #[napi]
    pub fn with_file<'env>(
        &mut self,
        this: This<'env>,
        path: String,
        #[napi(ts_arg_type = "Buffer | string")] content: Content,
    ) -> Result<This<'env>> {
        self.add_file(path, content)?;
        Ok(this)
    }

    /// `mount`, returning this same `Directory` so calls chain.
    #[napi]
    pub fn with_mount<'env>(
        &mut self,
        this: This<'env>,
        path: String,
        host_dir: String,
    ) -> Result<This<'env>> {
        self.mount(path, host_dir)?;
        Ok(this)
    }
}

/// A tree mounted on this host until `unmount()` or until nothing holds it.
///
/// Passing it to a console builder shares it rather than taking it: without `unmount()` it
/// comes down when the last holder lets go (the builder's copy with the console, this object
/// with garbage collection).
///
/// **Garbage collection is not an exit.** Node runs no finalizer on `process.exit()`, and none
/// on a signal or crash; a mount left to one is taken down from outside the process once it
/// is gone (by cortex's watchdog on unix, by Dokany on Windows). Call `unmount()` to take it
/// down *now*.
#[cfg(feature = "mount")]
#[napi(js_name = "HostMount")]
pub struct JsHostMount(Arc<Shared>);

/// The guard behind a `HostMount`, shared with every console it was handed to.
///
/// The `Option` lets `unmount` take the mount down while consoles still hold the `Arc`, making
/// it a real unmount rather than the release of one reference. The mount point is kept beside
/// it because consoles still ask for it afterwards.
#[cfg(feature = "mount")]
pub struct Shared {
    guard: std::sync::Mutex<Option<Platform>>,
    mountpoint: PathBuf,
}

#[cfg(feature = "mount")]
impl Mount for Shared {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

#[cfg(feature = "mount")]
#[napi]
impl JsHostMount {
    #[napi(constructor)]
    pub fn new(mut fs: ClassInstance<JsDirectory>, mountpoint: String) -> Result<Self> {
        let directory = fs.0.take().ok_or_else(taken)?;
        let mount = Platform::try_new(directory, Path::new(&mountpoint)).map_err(error::io)?;
        let mountpoint = mount.mountpoint().to_path_buf();
        Ok(JsHostMount(Arc::new(Shared {
            guard: std::sync::Mutex::new(Some(mount)),
            mountpoint,
        })))
    }

    #[napi(getter)]
    pub fn mountpoint(&self) -> String {
        self.0.mountpoint().to_string_lossy().into_owned()
    }

    /// Take the mount down now, and settle once it is down.
    ///
    /// Consoles it was handed to are left with an unmounted mount point, so call this after
    /// closing them. A repeat call, or one after the mount already came down, settles at once.
    ///
    /// Runs off the JavaScript thread: dropping a guard unmounts and then waits for the thread
    /// serving it, which waits for every holder of the tree to let go.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn unmount<'env>(
        &self,
        env: &'env napi::Env,
    ) -> napi::Result<napi::bindgen_prelude::PromiseRaw<'env, ()>> {
        let shared = self.0.clone();
        crate::console::promise(env, async move {
            let guard = shared
                .guard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(guard) = guard {
                tokio::task::spawn_blocking(move || drop(guard))
                    .await
                    .map_err(|e| error::invalid(format!("unmounting panicked: {e}")))?;
            }
            Ok(())
        })
    }
}

/// Throw, saying what to install, if this host cannot mount.
///
/// `HostMount` checks the same before mounting; this lets a caller find out ahead. An addon
/// built with `mount` still loads without FUSE-T or Dokany: only mounting fails, never `require`.
#[cfg(feature = "mount")]
#[napi]
pub fn mount_support() -> Result<()> {
    cortex::fs::mount_support().map_err(error::io)
}

/// What a console builder mounts: a `HostMount`, or a host directory by its path.
#[cfg(feature = "mount")]
pub type MountLike<'env> = Either<ClassInstance<'env, JsHostMount>, String>;
#[cfg(not(feature = "mount"))]
pub type MountLike = String;

pub fn into_mount(mount: MountLike) -> Result<Box<dyn Mount>> {
    #[cfg(feature = "mount")]
    let path = match mount {
        Either::A(mount) => return Ok(Box::new(mount.0.clone())),
        Either::B(path) => path,
    };
    #[cfg(not(feature = "mount"))]
    let path = mount;
    // Absolute, because the server receives a mount as a `file://` URL, and a relative path
    // would resolve against the process's working directory.
    Ok(Box::new(
        std::path::absolute(PathBuf::from(path)).map_err(error::io)?,
    ))
}
