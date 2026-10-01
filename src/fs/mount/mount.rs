use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// A filesystem the operating system has mounted, and a path where it will answer.
///
/// [`FileSystem`] and [`Directory`] describe a tree inside this process; nothing outside can
/// see them. A `Mount` is the state of having been mounted: a binding's guard, or a path the
/// host attached. Its one fact is [`mountpoint`](Self::mountpoint), a path any process on this
/// host can `open`, which is how anything outside this library (a guest included) reads a
/// virtx tree.
///
/// # The tree is there for as long as the value is
///
/// That is the whole contract, and it is a lower bound, not a lifecycle. What happens after
/// the value goes is the implementor's business (a guard unmounts, a temporary directory is
/// removed, a host directory stays) and is invisible through this trait. A guard meets the
/// bound by RAII: `try_new` mounts, `Drop` unmounts, and there is no `unmount` to forget or
/// call twice. To outlive one holder, share it ([`Arc`]) rather than leak it.
///
/// Guard-specific API such as `join` consumes the guard, so it cannot be on a `dyn Mount`.
///
/// A signal runs no destructor; [`unmount_on_signal`](crate::fs::unmount_on_signal) and
/// [`reclaim_abandoned`](crate::fs::reclaim_abandoned) cover that, deliberately not on this
/// trait.
///
/// [`Send`] + [`Sync`] because the holder is usually a task that keeps it across awaits and
/// hands out `&self` meanwhile.
///
/// [`FileSystem`]: crate::fs::FileSystem
/// [`Directory`]: crate::fs::Directory
pub trait Mount: Send + Sync {
    /// Where this is mounted — the directory a kernel now answers for.
    fn mountpoint(&self) -> &Path;

    /// This mount as a URL, e.g. `file:///srv/project`: the spelling given to anything outside
    /// the process, so it can be named alongside trees that are not local directories.
    ///
    /// Not percent-encoded, so a reader need not decode it back into a path.
    ///
    /// `None` unless the mount point is an absolute UTF-8 path: a relative path after
    /// `file://` reads as a host, and a lossy name would look valid but name another directory.
    fn url(&self) -> Option<String> {
        let mountpoint = self.mountpoint();
        let path = mountpoint.to_str().filter(|_| mountpoint.is_absolute())?;
        Some(format!("file://{path}"))
    }

    /// Where `path`, relative to the mounted tree's root, is on this host.
    ///
    /// `path` must be relative: `Path::join` with an absolute path discards the mountpoint.
    fn host_path(&self, path: &Path) -> PathBuf {
        self.mountpoint().join(path)
    }
}

/// A directory the host already mounted is a mount, and its own mount point.
///
/// Nothing here takes it down; the tree outlives every holder. This lets a plain directory
/// (`./out`, a caller's temp dir) stand in wherever a mount is expected.
///
/// Not canonicalized, since consumers report the spelling back and macOS would answer a temp
/// dir through `/private`. Not required to be absolute: only [`url`](Mount::url) needs that,
/// and it answers `None`.
impl Mount for PathBuf {
    fn mountpoint(&self) -> &Path {
        self.as_path()
    }
}

/// A boxed mount, including `Box<dyn Mount>` for a binding chosen at runtime.
impl<T: Mount + ?Sized> Mount for Box<T> {
    fn mountpoint(&self) -> &Path {
        (**self).mountpoint()
    }
}

/// A shared mount, including `Arc<dyn Mount>`. The tree stays up until the last holder drops.
impl<T: Mount + ?Sized> Mount for Arc<T> {
    fn mountpoint(&self) -> &Path {
        (**self).mountpoint()
    }
}
