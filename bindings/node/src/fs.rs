//! `Directory` and `HostMount`: the trees a session's commands see.
//!
//! A `Directory` is assembled in place, unlike an `Image`, because the Rust type is not
//! `Clone` — its files live in memory and a copy would be a second tree rather than a second
//! handle on one. Mounting it *takes* it: a `HostMount` owns the tree it serves, and the
//! `Directory` it was built from is empty afterwards and refuses further use.
//!
//! `HostMount` is one name for three types. cortex names each binding's guard after the
//! binding — `FuseMount`, `FuseTMount`, `DokanMount` — and only the one this platform has is
//! compiled, so a JavaScript caller who wants "mount this on the host" should not have to
//! know which it is.

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

    /// `addFile`, handing back this same `Directory` so calls chain.
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

    /// `mount`, handing back this same `Directory` so calls chain.
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

/// A tree mounted on this host, until `unmount` or until nothing holds it.
///
/// Held behind an [`Arc`] so that passing one to a console builder does not take it from
/// the JavaScript object: both hold the mount, and without an `unmount` it comes down when
/// the last of them lets go — the builder's copy with the console, the JavaScript one with
/// garbage collection.
///
/// **Garbage collection is not an exit.** Node runs no finalizer on `process.exit()`, and
/// none at all on a signal or a crash, so a mount left to one is taken down by cortex's
/// watchdog, from outside the process, once the process is gone. `unmount` is how a
/// program that wants it down *now* says so.
#[cfg(feature = "mount")]
#[napi(js_name = "HostMount")]
pub struct JsHostMount(Arc<Shared>);

/// The guard behind a `HostMount`, shared with every console it was handed to.
///
/// Held in an `Option` so that `unmount` can take it down while a console still holds the
/// `Arc` — which is what makes it an unmount rather than a release of one reference among
/// several. The mount point is kept beside it, since a console asks for it after as well.
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
    /// Whoever else holds it — a console it was handed to — holds a mount point that is no
    /// longer mounted from here on, so this belongs after the console using it is closed.
    /// A second call, or one after the mount already came down, settles at once.
    ///
    /// Off the JavaScript thread: a guard comes down by unmounting and then waiting for the
    /// thread serving it, which waits for every holder of the tree to let go.
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
/// `HostMount` checks the same before it mounts, so this is for a caller that wants to know
/// ahead of asking for one. An addon built with `mount` loads on a host without the
/// provider: what a missing FUSE-T or Dokany costs is a mount, never the `require`.
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
    // Absolute, because a mount is named to the server as a `file://` URL, and a relative
    // path from JavaScript is relative to wherever the process stands.
    Ok(Box::new(
        std::path::absolute(PathBuf::from(path)).map_err(error::io)?,
    ))
}
