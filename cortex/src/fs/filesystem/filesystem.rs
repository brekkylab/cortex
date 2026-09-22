//! What a store must provide to be exposed as a filesystem: [`FileSystem`], and the vocabulary
//! it answers in — [`Stat`], [`DirentKind`], [`Dirent`].
//!
//! Describing a tree is all of it. Whether one is *mounted* anywhere is
//! [`Mount`](crate::fs::Mount)'s, in [`mounts`](crate::fs) — see the module docs there for
//! why the two are not the same thing.
//!
//! Nothing a *caller's* open speaks is here: `OpenOptions` and `SetAttr` describe what a kernel
//! asked for, which no store ever sees, so they live with [`Posix`](crate::fs::Posix).
//!
//! # Every method hands back a boxed future
//!
//! The trait is meant to be held as a `dyn`: a mount table stores backends of
//! different concrete types behind one pointer. An `async fn` in a trait returns a
//! type only the implementation knows, which is a type a `dyn` cannot name — so every
//! method returns a [`BoxFuture`] instead, the same shape [`console::base`] uses and
//! for the same reason.
//!
//! One allocation per call, against a syscall or a round trip. `#[async_trait]` boxes
//! the identical future behind a macro; written out, the lifetimes are visible where
//! the borrows are, and a forwarding impl hands the inner future straight back
//! instead of wrapping it in a second box.
//!
//! Every method binds its borrows and its future to one lifetime, so a caller's
//! `path` and `buf` may be shorter-lived than the backend — which is the ordinary
//! case, since the buffer usually belongs to the request being served.
//!
//! [`Send`], because a mount is driven from whichever thread an interface binding
//! owns — which is also why the trait requires it of the backends themselves.
//!
//! [`console::base`]: crate::console

use std::{io, path::Path, sync::Arc, time::SystemTime};

use crate::BoxFuture;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirentKind {
    File,
    Dir,
}

/// Metadata about a single directory entry.
///
/// Fields beyond `kind`/`size` are optional because different backends expose
/// different subsets (local files, S3 objects, Notion pages, …).
#[derive(Clone, Debug)]
pub struct Stat {
    pub kind: DirentKind,

    pub size: u64,

    /// Last-modified time, if the backend reports one (S3 `LastModified`,
    /// Notion `last_edited_time`).
    pub mtime: Option<SystemTime>,

    /// Last-access time, if the backend reports one. Most object/document
    /// backends don't track access time, so this is usually `None`.
    pub atime: Option<SystemTime>,

    /// Change/creation time, if the backend reports one (Notion `created_time`).
    /// Not POSIX `ctime` exactly — the nearest timestamp the backend exposes.
    pub ctime: Option<SystemTime>,

    /// Birth/creation time, if the backend reports one (local files' `created`);
    /// `None` for providers that don't distinguish a birth time.
    pub created: Option<SystemTime>,

    /// Entity tag / content fingerprint, if available (S3 `ETag`).
    pub etag: Option<String>,

    /// Version id, if the backend is versioned (S3 `VersionId`).
    pub version: Option<String>,
}

impl Stat {
    /// Convenience constructor for a stat with only `kind` and `size` set and
    /// every optional field left as `None`.
    pub fn new(kind: DirentKind, size: u64) -> Self {
        Self {
            kind,
            size,
            mtime: None,
            atime: None,
            ctime: None,
            created: None,
            etag: None,
            version: None,
        }
    }
}

/// One entry in a directory listing.
pub struct Dirent {
    pub name: String,

    pub kind: DirentKind,

    /// Full metadata, but only when the listing produced it for free — which is a
    /// real property of the backend, hence its place in the type.
    ///
    /// An object store or document API returns sizes and timestamps in the *same*
    /// response, so filling this in costs nothing and saves the caller an N+1
    /// round trip (a FUSE `readdirplus`, a WebDAV `PROPFIND Depth: 1`). A local
    /// directory read yields names and `d_type` only, so `Some` there would mean
    /// an `lstat` per entry that a plain `ls` never asked for.
    ///
    /// Private, unlike the two fields above, because it is the one that can
    /// contradict them: `kind` also lives inside a [`Stat`], so a public field
    /// would let the two disagree. [`with_stat`](Self::with_stat) is the only way
    /// to set it and takes `kind` *from* the stat, which is what makes the
    /// disagreement unrepresentable rather than merely undocumented.
    stat: Option<Stat>,
}

impl Dirent {
    /// An entry whose metadata the listing did not include.
    pub fn new(name: impl Into<String>, kind: DirentKind) -> Self {
        Dirent {
            name: name.into(),
            kind,
            stat: None,
        }
    }

    /// An entry the listing already knew everything about.
    pub fn with_stat(name: impl Into<String>, stat: Stat) -> Self {
        Dirent {
            name: name.into(),
            kind: stat.kind,
            stat: Some(stat),
        }
    }

    /// The metadata the listing came with, if it came with any.
    ///
    /// `None` is not "unknown to the backend" — it is "not free from the listing".
    /// A consumer that needs it anyway asks [`FileSystem::stat`](crate::fs::FileSystem::stat),
    /// and pays for it.
    pub fn stat(&self) -> Option<&Stat> {
        self.stat.as_ref()
    }
}

/// A logical, path-addressed store that can be exposed as a filesystem.
///
/// A *description* of a tree and not a mounted one: implementing this makes something
/// answerable about names and bytes, and nothing about it is visible outside this process
/// until a binding puts it behind a real filesystem interface. That step is
/// [`Mount`](crate::fs::Mount), which is the mounted state itself.
///
/// Every operation names its target by path, and there are only two kinds of them:
/// the namespace (`stat`/`list`/`create`/`mkdir`/`unlink`/`rmdir`/`rename`) and the
/// bytes (`read_at`/`write_at`/`truncate`/`flush`). Nothing stands between a caller
/// and either one:
///
/// * The trait stays object-safe, so `dyn FileSystem` is the whole of erasure. A
///   trait handing out an associated handle type needs a hand-written object-safe
///   twin and a blanket impl bridging the two before a heterogeneous mount table can
///   hold two backends at once — and then a third impl to make the erased handle
///   usable as a handle.
/// * It is the shape a wire already has. The console's `read`/`write` carry a path, an
///   offset and a length, so a backend on the far side of a channel forwards its calls
///   rather than keeping a second bookkeeping of its own on each end.
/// * Most backends have no per-open state to keep. Their bytes are reachable by path,
///   and an open that returned something would only be handing back a slice of a cache
///   the backend already holds.
///
/// # No opens, only paths
///
/// A store answers about names and bytes. Nothing here is a descriptor: there is no
/// handle to hold, no id to key state by, no "the last one closed" to wait for, and no
/// `open` — a POSIX open decomposes into the parts a store *does* have, and the parts
/// are all above. `O_CREAT|O_EXCL` is [`create`](Self::create), which is the only way
/// to make a name exclusively; `O_TRUNC` is [`truncate`](Self::truncate); the rest of
/// an open is bookkeeping in the layer that has descriptors. That is what keeps a
/// backend author — writing a provider for a document API, an object store, a database
/// — from having to reason about an identity POSIX invented and this trait's other
/// consumers do not have.
///
/// What that identity buys is paid for where the descriptors already are. A kernel
/// binding owns the file-handle numbers and the kernel's reference counts, so an
/// open's semantics are its to keep: amortizing the expensive part of an open, and the
/// rule that a descriptor outlives the name it was resolved from, belong to that layer
/// — which can hold the name alive until the last reference goes, the way NFS does,
/// without any backend knowing.
///
/// A backend must not try to keep that state itself, and a path is the reason why. A
/// path names a file; it does not name an open of one, and the two come apart wherever
/// POSIX lets a name be unlinked and made again while an earlier open is still being
/// written. State keyed by path would then serve the second open's bytes to the first
/// one's reads, without erring — so a plain path plane is not merely simpler here, it
/// is the only version a backend can get right.
///
/// # Three methods are required; everything that mutates refuses by default
///
/// [`stat`](Self::stat), [`list`](Self::list) and [`read_at`](Self::read_at) have no
/// default, because a store that cannot answer those is not one. Every method that
/// changes something defaults to [`ReadOnlyFilesystem`], so a read-only backend — an
/// object store, a page API — implements the three and stops.
///
/// A writable backend that leaves one of them out therefore answers `EROFS` where it
/// meant to work. That is a loud failure on the first write and not a silent one, and
/// it is the trade for read-only backends not spelling out seven refusals each.
///
/// [`ReadOnlyFilesystem`]: io::ErrorKind::ReadOnlyFilesystem
///
/// # Durability
///
/// **A write is durable when it returns.** [`write_at`](Self::write_at) answers once
/// the store has the bytes; there is no later call in which to finish the job, because
/// there is no signal for when a caller is done. A backend that must batch does so on
/// its own terms — a size threshold, a timer — and not on a lifecycle the trait does
/// not have.
///
/// [`flush`](Self::flush) is the one thing next to that, and it is about a name rather
/// than an opener: make what is written durable *now*. It exists so a guest's `fsync`
/// has somewhere to land instead of a silent `Ok`, and defaults to exactly that `Ok`
/// for a store where the write already was.
///
/// # What a store answers with
///
/// [`io::Error`], and by [`kind`](io::Error::kind) rather than by an errno: the kinds
/// carry every distinction this crate needs, an error from `std::fs` travels up with
/// its message and its `source` intact, and one from a backend's own client wraps with
/// [`io::Error::other`] rather than flattening into a variant that forgets what
/// happened.
///
/// Three are easy to get wrong, and userspace acts on the difference:
///
/// * [`ReadOnlyFilesystem`] is a store that could write and will not (`EROFS`).
///   [`Unsupported`] is `ENOSYS` — "this filesystem does not implement the operation
///   at all" — and `cp`, `rsync` and editors have a read-only path for the first and
///   none for the second, which reads as a filesystem that is broken rather than as
///   data that is protected. `ENOSYS` also does *not* make a kernel stop asking:
///   measured on a Linux guest over virtio-fs, `mkdir`, `unlink`, `rmdir`, `rename`
///   and `write` each reached the backend on all three of three attempts.
/// * [`CrossesDevices`] is `EXDEV`, and it is not a dead end: "copy, then delete" has
///   been the answer for decades and `mv`, `rsync` and editors all implement it, so
///   naming it precisely is what lets a cross-backend move *succeed*.
/// * [`PermissionDenied`] is `EACCES`, which `find`/`rsync`/`tar` skip on where they
///   abort on `EIO` — so collapsing the two loses a whole traversal to one unreadable
///   file.
///
/// A consumer translating these for a kernel classifies by `kind`. Whether it may
/// also pass [`raw_os_error`](io::Error::raw_os_error) through depends on who reads
/// the number: a host mount and this process share a numbering, so forwarding one is
/// both correct and more precise than the kind it was classified into. A **guest**
/// does not — `ENOTEMPTY` is 66 on macOS against 39 on Linux — so a binding whose
/// reader is a Linux guest must carry its own table and forward nothing.
///
/// [`Unsupported`]: io::ErrorKind::Unsupported
/// [`CrossesDevices`]: io::ErrorKind::CrossesDevices
/// [`PermissionDenied`]: io::ErrorKind::PermissionDenied
///
/// # Async
///
/// The namespace and data planes are naturally async (an object store, a document
/// API), so the trait is too. An async-native consumer (a WebDAV/HTTP frontend)
/// `.await`s these directly; a sync interface binding (fuse/fuse-t) `block_on`s
/// them at its callback boundary.
///
/// Awaiting directly is only truly non-blocking for backends whose I/O is actually
/// async. One that calls blocking `std::fs`, or locks a map, is async in signature
/// only: it never yields, so driving it from an executor thread stalls that worker for
/// the syscall. An async-native frontend over such a backend should wrap the calls in
/// `tokio::task::block_in_place` (gated on a multi-thread runtime), which costs
/// nothing when the call is already quick, rather than `spawn_blocking`.
pub trait FileSystem: Send + Sync {
    /// Metadata for one entry (works on files *and* directories).
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>>;

    /// The entries directly under `path`.
    ///
    /// Each entry carries metadata only if the listing already had it; see
    /// [`Dirent::stat`].
    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>>;

    /// Read into `buf` at `offset`, returning the bytes read.
    ///
    /// **A short return means EOF and nothing else.** A consumer passes the length
    /// straight on to whatever asked for it, so a backend that could serve only part
    /// of the request from a cache has to fill the rest before answering — or the file
    /// appears to end at a cache boundary.
    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>>;

    /// Create an empty file at `path`, and report the metadata it starts with.
    ///
    /// **Exclusive.** [`AlreadyExists`](io::ErrorKind::AlreadyExists) if the name is
    /// taken, whatever is under it. That is not a policy choice: `O_EXCL` is a
    /// guarantee only the store can make, and a caller that merely wanted the file to
    /// exist reads the error and carries on, where one that needed to win the race has
    /// no other way to learn it did.
    ///
    /// The [`Stat`] comes back because it is free here and a round trip otherwise: a
    /// kernel's `create` answers with attributes in the same message that reports the
    /// new entry, and asking again would leave a window for the name to be replaced.
    ///
    /// The parent is never created. A caller building a tree calls
    /// [`mkdir`](Self::mkdir) for each level, which is what its own `ENOENT` tells it
    /// to do.
    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        let _ = path;
        Box::pin(async { Err(io::ErrorKind::ReadOnlyFilesystem.into()) })
    }

    /// Create a directory at `path`, and report the metadata it starts with — the
    /// [`create`](Self::create) contract, for the other kind of entry.
    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        let _ = path;
        Box::pin(async { Err(io::ErrorKind::ReadOnlyFilesystem.into()) })
    }

    /// Remove the *file* at `path`; a directory is rejected with [`IsADirectory`]. Use
    /// [`rmdir`] for those.
    ///
    /// Split because a filesystem never deletes recursively: `rm -rf` is decomposed by
    /// the caller into `list`, an `unlink` per file, and a final `rmdir`. A backend
    /// that quietly removed a subtree here would only ever be reached by mistake.
    ///
    /// The name is all this is about. Whether something reading the file may go on
    /// reading it is not a question a store can answer — see *No opens, only paths*.
    ///
    /// [`IsADirectory`]: io::ErrorKind::IsADirectory
    /// [`rmdir`]: Self::rmdir
    fn unlink<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        let _ = path;
        Box::pin(async { Err(io::ErrorKind::ReadOnlyFilesystem.into()) })
    }

    /// Remove the *empty directory* at `path`.
    ///
    /// A file is rejected with [`NotADirectory`], and a directory that still has
    /// children with [`DirectoryNotEmpty`].
    ///
    /// [`NotADirectory`]: io::ErrorKind::NotADirectory
    /// [`DirectoryNotEmpty`]: io::ErrorKind::DirectoryNotEmpty
    fn rmdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        let _ = path;
        Box::pin(async { Err(io::ErrorKind::ReadOnlyFilesystem.into()) })
    }

    /// Write `buf` at `offset`, zero-extending the file if needed; returns the bytes
    /// written.
    ///
    /// A short write is legal, and draining the buffer is the consumer's job — one
    /// loop, written once above the trait, rather than a whole-buffer default here
    /// that a backend could override into disagreement with it.
    ///
    /// The file must exist: this never creates one, so that a write to a name that
    /// went away is an error rather than a resurrection. [`create`](Self::create) is
    /// the one thing that makes a name.
    fn write_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        let _ = (path, buf, offset);
        Box::pin(async { Err(io::ErrorKind::ReadOnlyFilesystem.into()) })
    }

    /// Resize the file to `size` bytes, zero-filling any growth.
    ///
    /// Both resizes a filesystem has: a `setattr` carrying a size, and the `O_TRUNC`
    /// that rides an open — a consumer with one calls this before anything can observe
    /// the file, which is the whole of what `O_TRUNC` promises.
    fn truncate<'a>(&'a self, path: &'a Path, size: u64) -> BoxFuture<'a, io::Result<()>> {
        let _ = (path, size);
        Box::pin(async { Err(io::ErrorKind::ReadOnlyFilesystem.into()) })
    }

    /// Move the entry at `from` to `to`, replacing whatever was there.
    ///
    /// No flags. `RENAME_NOREPLACE`/`RENAME_EXCHANGE` reach two of the three bindings
    /// but libfuse-t's `rename` has no flags argument at all, so a contract carrying
    /// them could not be honoured everywhere; the bindings that receive them answer
    /// `EINVAL`, as Linux does for a flag it cannot serve.
    ///
    /// Both paths belong to *this* backend. A move that crosses a mount boundary is
    /// [`CrossesDevices`](io::ErrorKind::CrossesDevices), decided a layer up where the
    /// mount table is visible.
    ///
    /// Overwrite rules follow `rename(2)`, which a local backend gets for free from
    /// `fs::rename`: a file replaces a file, a directory replaces an *empty* directory,
    /// and the mismatched pairs are `EISDIR`/`ENOTDIR`/`ENOTEMPTY`. Renaming a path
    /// onto itself succeeds without doing anything, and moving a directory inside
    /// itself is `EINVAL`.
    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        let _ = (from, to);
        Box::pin(async { Err(io::ErrorKind::ReadOnlyFilesystem.into()) })
    }

    /// Make what is written to `path` durable now — a guest's `fsync`.
    ///
    /// Not a "done" signal, and there is none: nothing tells a store that the last
    /// writer has gone away, so this may arrive mid-stream, many times, and after the
    /// final write indistinguishably. A backend must not read it as the finished file.
    ///
    /// The one mutating method whose default is `Ok`, because it is not a mutation: a
    /// store whose writes were durable when they returned has already done what this
    /// asks — see *Durability*.
    fn flush<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        let _ = path;
        Box::pin(async { Ok(()) })
    }

    /// Drop whatever is being kept, so the next read asks the source again.
    ///
    /// For a store that keeps nothing this is what it looks like: nothing. It is for the
    /// ones that do — a remote store that renders or caches, where a reader who can see
    /// that the source has moved on has no other way to say so. Consistency is not what
    /// this is for: a cache that could be wrong in a way a `stat` would catch does not need
    /// asking. It is for what a store cannot check cheaply, and it arrives when a person
    /// asks for it.
    ///
    /// Not a failure to report: a store that cannot drop something has already answered the
    /// question by keeping it, and a refresh that returns an error the caller cannot act on
    /// is a worse answer than one that quietly did what it could.
    fn forget<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// A shared backend is itself a backend: every call forwards to the one inside.
///
/// `?Sized`, so one impl covers `Arc<dyn FileSystem>` as well as `Arc<T>` — the
/// erased case needs it as much as the concrete one, and a trait object cannot get it
/// from a second, overlapping impl.
///
/// Needed because each consumer takes its backend by value and offers no way back.
/// Without this a store feeds exactly one consumer, so one workspace could not be
/// served to an agent through a host mount and to a person over HTTP at the same time.
/// Nothing about sharing has to be arranged: with no opens to keep straight, two
/// consumers are only two callers.
///
/// Each method hands the inner future back untouched rather than awaiting it inside
/// one of its own, so sharing a backend costs no allocation on top of the call.
impl<T: FileSystem + ?Sized> FileSystem for Arc<T> {
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        (**self).stat(path)
    }

    fn forget<'a>(&'a self) -> BoxFuture<'a, ()> {
        (**self).forget()
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        (**self).list(path)
    }

    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        (**self).read_at(path, buf, offset)
    }

    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        (**self).create(path)
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        (**self).mkdir(path)
    }

    fn unlink<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        (**self).unlink(path)
    }

    fn rmdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        (**self).rmdir(path)
    }

    fn write_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        (**self).write_at(path, buf, offset)
    }

    fn truncate<'a>(&'a self, path: &'a Path, size: u64) -> BoxFuture<'a, io::Result<()>> {
        (**self).truncate(path, size)
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        (**self).rename(from, to)
    }

    fn flush<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        (**self).flush(path)
    }
}
