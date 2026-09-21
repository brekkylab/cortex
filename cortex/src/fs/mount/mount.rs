use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// A filesystem the operating system has mounted, and a path where it will answer.
///
/// # Not a filesystem — a mounted one
///
/// [`FileSystem`] and [`ContextFs`] *describe* a tree: they are contracts and types in this
/// process, saying what is under which name and what its bytes are. Nothing outside the
/// process can see either of them. This is the other thing entirely — the state of having
/// been mounted. What implements it is not a store but that state: the guard a binding hands
/// back once a kernel has attached a tree, or a path on one the host attached long before
/// this process started. What it offers is the one fact only that state has:
/// [`mountpoint`](Self::mountpoint), a path any process on this host can `open`.
///
/// So the two compose rather than overlap. A `ContextFs` is what a mount serves; a `Mount` is
/// what makes it reachable by name — which is how anything that is not this library reads a
/// cortex tree, a guest included, and why a consumer that needs real paths takes one of these
/// and not a store.
///
/// # The tree is there for as long as the value is
///
/// **That is the whole of the contract**, and it is a lower bound rather than a lifecycle: a
/// holder may `open` under the mount point at any moment it still has the value, and the tree
/// will not have gone out from under it. What happens *after* the value goes is the
/// implementor's own business — a binding's guard unmounts, a temporary directory is removed,
/// a directory the host already had stays exactly as it stood. None of those is visible
/// through this trait, and nothing consuming a mount asks which one it holds.
///
/// So the two kinds of implementor meet the bound from opposite ends and are equally mounts
/// here. A tree this process put up meets it by RAII — `try_new` mounts, the value *is* the
/// mount, `Drop` takes it down, and there is no `unmount` to forget and none to call twice —
/// and holding the value is then the only thing keeping the tree up. A tree the host already
/// had meets it because nothing that happens to the value can affect the tree at all.
///
/// A caller that wants a mount of the first kind to outlive one holder shares it ([`Arc`],
/// impl below) rather than leaking it: the bound then runs to the last holder, which is the
/// same rule read once more.
///
/// What a guard adds beyond this trait is its own — `join`, waiting for something else to end
/// the mount, consumes the guard and so is not something a `dyn Mount` could offer.
///
/// # The one exit that runs no destructor
///
/// A signal does not drop anything. The process stops between two instructions, every
/// destructor it owed goes unrun, and the mount is left registered with nothing answering
/// it — which is worse than a leak, because a path that no longer answers stops everything
/// that walks it. That is not a hole in the rule above so much as the rule having nothing to
/// stand on: there is no destructor to be the lifecycle.
///
/// Two things cover it, and they are deliberately not on this trait:
///
/// * [`unmount_on_signal`](crate::fs::unmount_on_signal), for the signals that can be caught.
///   **Opt-in**, because a signal disposition is process-global and a library that claimed
///   one would be overwriting whatever the program embedding it had arranged.
/// * [`reclaim_abandoned`](crate::fs::reclaim_abandoned), for `SIGKILL`, which reaches no
///   handler at all. Nothing the dying process does can help, so the mount is reclaimed by
///   whoever comes next — which is why it is a free function rather than a method on a guard
///   that no longer exists, and why every `try_new` calls it so that nobody has to.
///
/// [`Send`] + [`Sync`], because a mount is held for as long as it serves and the holder is
/// usually a task: a console keeps one across every await in an execution, and hands `&self`
/// to whatever runs in the meantime.
///
/// [`FileSystem`]: crate::fs::FileSystem
/// [`ContextFs`]: crate::fs::ContextFs
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
    /// hands around are relative for that reason.
    fn host_path(&self, path: &Path) -> PathBuf {
        self.mountpoint().join(path)
    }
}

/// A path is a mount, and is its own mount point.
///
/// The host mounted this one — at boot, or whenever whatever holds the directory was
/// attached — and it is under *that* mount that a kernel answers for the path. None of it is
/// this library's to take down, which is why a `PathBuf` satisfies the rule above by doing
/// nothing in `Drop`: the tree outlives every holder, and what the rule asks is that it
/// outlive this one.
///
/// **What it buys is a directory being enough.** A console left to put its artifacts in
/// `./out`, or to work in a temporary directory the caller already made, is being handed a
/// tree the host can reach with nothing to mount — and without this impl every such caller
/// writes the same newtype around the same `PathBuf` to say so.
///
/// The path is taken as it stands. It is not canonicalized, because the spelling handed over
/// is the one a consumer reports back, and canonicalizing a temporary directory on macOS
/// answers through `/private` a name the caller never used. It is not required to be absolute
/// either: [`url`](Mount::url) is where that matters and already answers `None`, so a caller
/// that names a relative directory hears about it where the path has to travel, and one that
/// only ever joins onto it locally is not stopped over a rule it is not relying on.
impl Mount for PathBuf {
    fn mountpoint(&self) -> &Path {
        self.as_path()
    }
}

/// A boxed mount is still a mount, and reports the same path.
///
/// `?Sized`, so this covers `Box<dyn Mount>` as well as `Box<T>`: a caller that chooses its
/// binding at runtime holds that choice erased, and passes it on as a `Mount` without having to
/// unwrap it first.
///
/// A box has one owner, so nothing about the rule above changes — whatever the mount inside
/// does when it goes happens when the box drops.
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
/// tree — without either of them deciding when it comes down. The last `Arc` to go is what
/// ends it, so the rule above holds for the group rather than being weakened by it: the tree
/// is there for as long as any holder has one.
impl<T: Mount + ?Sized> Mount for Arc<T> {
    fn mountpoint(&self) -> &Path {
        (**self).mountpoint()
    }
}
