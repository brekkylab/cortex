use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// A filesystem the operating system has mounted, and a path where it will answer.
///
/// # Not a filesystem — a mounted one
///
/// [`FileSystem`] and [`WorkFs`] *describe* a tree: they are contracts and types in this
/// process, saying what is under which name and what its bytes are. Nothing outside the
/// process can see either of them. This is the other thing entirely — the state of having
/// been mounted. What implements it is not a store but the guard a binding hands back once a
/// kernel has attached one, and what it offers is the one fact only that state has:
/// [`mountpoint`](Self::mountpoint), a path any process on this host can `open`.
///
/// So the two compose rather than overlap. A `WorkFs` is what a mount serves; a `Mount` is
/// what makes it reachable by name — which is how anything that is not this library reads a
/// cortex tree, a guest included, and why a consumer that needs real paths takes one of these
/// and not a store.
///
/// # A mount lives exactly as long as the value
///
/// **Dropping it unmounts.** That is the contract, not an incidental property of the two
/// bindings that have it: an implementor whose `Drop` leaves the mount up is not a `Mount`,
/// because a caller holding one would have no way to take it down and no way to know it was
/// still there. RAII is the whole lifecycle here — `try_new` mounts, the value *is* the
/// mount, and there is no `unmount` to forget and none to call twice.
///
/// A caller that wants the mount to outlive one holder shares it ([`Arc`], impl below) rather
/// than leaking it: the mount then comes down when the last holder goes, which is still the
/// same rule.
///
/// What a guard adds beyond this trait is its own — `join`, waiting for something else to end
/// the mount, consumes the guard and so is not something a `dyn Mount` could offer.
///
/// [`Send`] + [`Sync`], because a mount is held for as long as it serves and the holder is
/// usually a task: a console keeps one across every await in an execution, and hands `&self`
/// to whatever runs in the meantime.
///
/// [`FileSystem`]: crate::fs::FileSystem
/// [`WorkFs`]: crate::fs::WorkFs
pub trait Mount: Send + Sync {
    /// Where this is mounted — the directory a kernel now answers for.
    fn mountpoint(&self) -> &Path;

    /// This mount, named as a URL — `file:///srv/project`.
    ///
    /// The spelling of a mount point that travels. A path is a fact about this host, where a
    /// URL is a name for a *tree* and says which kind of thing it is by its scheme — so
    /// anything being told about this mount from outside the process is told this, and a
    /// tree that is not a directory on this host can be named alongside one that is.
    ///
    /// The path goes in as it stands and is not percent-encoded: a reader would have to
    /// decode it before it was a path again, which is a second thing to get right about one
    /// directory.
    ///
    /// `None` when the mount point is not an absolute UTF-8 path. Both halves of that are
    /// what a URL cannot do without rather than a restriction added here — a relative path
    /// after `file://` reads as a host, and a lossy name is a directory that looks valid and
    /// is not this one.
    fn url(&self) -> Option<String> {
        let mountpoint = self.mountpoint();
        let path = mountpoint.to_str().filter(|_| mountpoint.is_absolute())?;
        Some(format!("file://{path}"))
    }

    /// Where `path`, named relative to the root of the mounted tree, is on this host.
    ///
    /// The translation every consumer of a mount performs, provided once because the join is
    /// only correct for a *relative* path: `Path::join` given an absolute one throws the
    /// mountpoint away and answers about the host's own root instead. The paths this crate
    /// hands around are relative for that reason — see
    /// [`resolve_under`](crate::exec::resolve_under), which is where an argument from
    /// outside becomes one.
    fn host_path(&self, path: &Path) -> PathBuf {
        self.mountpoint().join(path)
    }
}

/// A boxed mount is still a mount, and reports the same path.
///
/// `?Sized`, so this covers `Box<dyn Mount>` as well as `Box<T>`: a caller that chooses its
/// binding at runtime holds that choice erased, and passes it on as a `Mount` without having to
/// unwrap it first.
///
/// A box has one owner, so nothing about the rule above changes — the unmount happens when the
/// box drops, which is when the mount inside it does.
impl<T: Mount + ?Sized> Mount for Box<T> {
    fn mountpoint(&self) -> &Path {
        (**self).mountpoint()
    }
}

/// A shared mount is still a mount, and reports the same path.
///
/// `?Sized`, so this covers `Arc<dyn Mount>` as well as `Arc<T>`: a consumer that stores one
/// erased still has something it can pass on as a `Mount`.
///
/// Sharing is how a mount serves two holders — whoever mounted, and whatever was handed the
/// tree — without either of them deciding when it comes down. The unmount happens when the
/// last `Arc` goes, so the RAII rule above holds for the group rather than being weakened by
/// it.
impl<T: Mount + ?Sized> Mount for Arc<T> {
    fn mountpoint(&self) -> &Path {
        (**self).mountpoint()
    }
}
