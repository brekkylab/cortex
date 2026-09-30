//! What a store provides to be exposed as a filesystem: [`FileSystem`] and its vocabulary,
//! [`Stat`], [`DirentKind`], [`Dirent`].
//!
//! This only describes a tree; being *mounted* is [`Mount`](crate::fs::Mount)'s concern (see
//! [`fs`](crate::fs)). `OpenOptions` and `SetAttr` describe what a kernel asked for, which no
//! store sees, so they live with [`Posix`](crate::fs::Posix).
//!
//! # Every method returns a boxed future
//!
//! The trait is held as `dyn` (a mount table stores backends of different types behind one
//! pointer), and an `async fn` in a trait returns a type a `dyn` cannot name, so every method
//! returns a [`BoxFuture`].
//!
//! That is one allocation per call, against a syscall or round trip. Written out rather than
//! via `#[async_trait]`, the lifetimes are visible where the borrows are, and a forwarding impl
//! returns the inner future instead of boxing it again.
//!
//! Each method binds its borrows and future to one lifetime, so `path` and `buf` may be
//! shorter-lived than the backend, as they usually are: the buffer belongs to the request.
//!
//! [`Send`] because a mount is driven from whichever thread its binding owns; hence backends
//! must be `Send` too.

use std::{io, path::Path, sync::Arc, time::SystemTime};

use crate::BoxFuture;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirentKind {
    File,
    Dir,
}

/// Metadata about a single directory entry.
///
/// Fields beyond `kind`/`size` are optional because backends expose different subsets.
#[derive(Clone, Debug)]
pub struct Stat {
    pub kind: DirentKind,

    pub size: u64,

    /// Last-modified time, if the backend reports one (S3 `LastModified`,
    /// Notion `last_edited_time`).
    pub mtime: Option<SystemTime>,

    /// Last-access time; usually `None`, since most object/document backends don't track it.
    pub atime: Option<SystemTime>,

    /// The backend's nearest timestamp to POSIX `ctime`, if any (Notion `created_time`).
    pub ctime: Option<SystemTime>,

    /// Birth time, if the backend distinguishes one (local files' `created`).
    pub created: Option<SystemTime>,

    /// Entity tag / content fingerprint, if available (S3 `ETag`).
    pub etag: Option<String>,

    /// Version id, if the backend is versioned (S3 `VersionId`).
    pub version: Option<String>,
}

impl Stat {
    /// A stat with only `kind` and `size`; every optional field `None`.
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
///
/// `Clone` so a store can cache a listing and hand the same entries out again.
#[derive(Clone, Debug)]
pub struct Dirent {
    pub name: String,

    pub kind: DirentKind,

    /// Full metadata, only when the listing produced it for free.
    ///
    /// An object store or document API returns sizes and timestamps in the listing response,
    /// saving an N+1 round trip (FUSE `readdirplus`, WebDAV `PROPFIND Depth: 1`). A local
    /// directory read yields only names and `d_type`, so `Some` there would cost an `lstat` per
    /// entry a plain `ls` never asked for.
    ///
    /// Private so it cannot contradict `kind`: [`with_stat`](Self::with_stat), the only setter,
    /// takes `kind` *from* the stat.
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
    /// `None` means "not free from the listing", not "unknown"; ask
    /// [`FileSystem::stat`] if it is needed.
    pub fn stat(&self) -> Option<&Stat> {
        self.stat.as_ref()
    }
}

/// A logical, path-addressed store that can be exposed as a filesystem.
///
/// A *description* of a tree: nothing about it is visible outside this process until a binding
/// puts it behind a real filesystem interface, yielding a [`Mount`](crate::fs::Mount).
///
/// Every operation names its target by path, in two planes: the namespace
/// (`stat`/`list`/`create`/`mkdir`/`unlink`/`rmdir`/`rename`) and the bytes
/// (`read_at`/`write_at`/`truncate`/`flush`). Nothing stands between a caller and either:
///
/// * The trait stays object-safe, so `dyn FileSystem` is all the erasure needed. An associated
///   handle type would need an object-safe twin, a bridging blanket impl, and a third impl to
///   make the erased handle usable before a mount table could hold mixed backends.
/// * It matches the wire: the console's `read`/`write` carry path, offset and length, so a
///   remote backend forwards calls instead of keeping bookkeeping on each end.
/// * Most backends have no per-open state; their bytes are reachable by path, and an open
///   handle would only return a slice of a cache they already hold.
///
/// # No opens, only paths
///
/// Nothing here is a descriptor: no handle, no id to key state by, no "last close", no `open`.
/// A POSIX open decomposes into what a store does have: `O_CREAT|O_EXCL` is
/// [`create`](Self::create) (the only exclusive way to make a name), `O_TRUNC` is
/// [`truncate`](Self::truncate), and the rest is bookkeeping in the layer that has
/// descriptors. So a backend author (document API, object store, database) never reasons about
/// an identity POSIX invented.
///
/// That identity is kept where descriptors already are. A kernel binding owns file-handle
/// numbers and the kernel's reference counts, so it amortizes the expensive part of an open and
/// keeps a descriptor alive past its name's unlink (as NFS does), without any backend knowing.
///
/// A backend must not keep that state itself: a path names a file, not an open of it, and POSIX
/// lets a name be unlinked and recreated while an earlier open is still written. Path-keyed
/// state would then silently serve the second file's bytes to the first open's reads.
///
/// # Three methods are required; everything that mutates refuses by default
///
/// [`stat`](Self::stat), [`list`](Self::list) and [`read_at`](Self::read_at) have no default.
/// Every mutating method defaults to [`ReadOnlyFilesystem`], so a read-only backend (object
/// store, page API) implements just the three. A writable backend that forgets one answers
/// `EROFS` loudly on first use instead of failing silently.
///
/// [`ReadOnlyFilesystem`]: io::ErrorKind::ReadOnlyFilesystem
///
/// # Durability
///
/// **A write is durable when it returns.** [`write_at`](Self::write_at) answers once the store
/// has the bytes, since no signal says a caller is done. A backend that must batch does so on
/// its own terms (size threshold, timer).
///
/// [`flush`](Self::flush) makes what is written to a name durable *now*. It gives a guest's
/// `fsync` somewhere to land instead of a silent `Ok`, and defaults to `Ok` for stores whose
/// writes already were durable.
///
/// # Errors
///
/// [`io::Error`], classified by [`kind`](io::Error::kind) rather than errno: kinds carry every
/// distinction this crate needs, `std::fs` errors keep their message and `source`, and a
/// backend client's error wraps with `io::Error::other` instead of being flattened.
///
/// Three are easy to get wrong, and userspace acts on the difference:
///
/// * [`ReadOnlyFilesystem`] (`EROFS`) is a store that could write and will not;
///   [`Unsupported`] is `ENOSYS`, "not implemented at all". `cp`, `rsync` and editors handle
///   the first and read the second as a broken filesystem. `ENOSYS` also does *not* stop a
///   kernel asking: a Linux guest over virtio-fs keeps sending `mkdir`, `unlink`, `rmdir`,
///   `rename` and `write` after it.
/// * [`CrossesDevices`] (`EXDEV`) makes `mv`, `rsync` and editors copy then delete, so naming
///   it precisely lets a cross-backend move *succeed*.
/// * [`PermissionDenied`] (`EACCES`) is skipped by `find`/`rsync`/`tar`, which abort on `EIO`;
///   collapsing the two loses a whole traversal to one unreadable file.
///
/// A consumer translating these for a kernel classifies by `kind`. Forwarding
/// [`raw_os_error`](io::Error::raw_os_error) is correct and more precise for a host mount,
/// which shares this process's numbering, but not for a **guest** (`ENOTEMPTY` is 66 on macOS,
/// 39 on Linux), so a binding read by a Linux guest must use its own table and forward nothing.
///
/// [`Unsupported`]: io::ErrorKind::Unsupported
/// [`CrossesDevices`]: io::ErrorKind::CrossesDevices
/// [`PermissionDenied`]: io::ErrorKind::PermissionDenied
///
/// # Async
///
/// Both planes are naturally async (object store, document API). An async-native consumer
/// (WebDAV/HTTP frontend) `.await`s directly; a sync binding (fuse/fuse-t) `block_on`s at its
/// callback boundary.
///
/// A backend calling blocking `std::fs` or locking a map is async in signature only and stalls
/// the executor worker for the syscall. An async-native frontend over one should wrap calls in
/// `tokio::task::block_in_place` (multi-thread runtime only), free when the call is quick,
/// rather than `spawn_blocking`.
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
    /// **A short return means EOF and nothing else.** Consumers pass the length straight on,
    /// so a backend serving part of a request from cache must fill the rest first, or the file
    /// appears to end at a cache boundary.
    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>>;

    /// Create an empty file at `path`, and report the metadata it starts with.
    ///
    /// **Exclusive**: [`AlreadyExists`](io::ErrorKind::AlreadyExists) if the name is taken by
    /// anything. `O_EXCL` is a guarantee only the store can make; a caller that just wanted the
    /// file to exist ignores the error, while one racing has no other way to learn it won.
    ///
    /// The [`Stat`] is returned because a kernel's `create` reply carries attributes, and a
    /// second `stat` would leave a window for the name to be replaced.
    ///
    /// The parent is never created; `ENOENT` tells a caller to [`mkdir`](Self::mkdir) each level.
    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        let _ = path;
        Box::pin(async { Err(io::ErrorKind::ReadOnlyFilesystem.into()) })
    }

    /// Create a directory at `path` and report its initial metadata. Exclusive, and the parent
    /// is never created.
    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        let _ = path;
        Box::pin(async { Err(io::ErrorKind::ReadOnlyFilesystem.into()) })
    }

    /// Remove the *file* at `path`; a directory is rejected with [`IsADirectory`]. Use
    /// [`rmdir`] for those.
    ///
    /// A filesystem never deletes recursively: callers decompose `rm -rf` into `list`, `unlink`
    /// per file and a final `rmdir`, so a subtree removal here would only be reached by mistake.
    ///
    /// This removes only the name; whether an existing reader may continue is not the store's
    /// question (see *No opens, only paths*).
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
    /// A short write is legal; the consumer drains the buffer, in one loop above the trait
    /// rather than a whole-buffer default a backend could override inconsistently.
    ///
    /// Never creates the file, so writing to a name that went away is an error, not a
    /// resurrection. Only [`create`](Self::create) makes a name.
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
    /// Serves both a `setattr` size and an open's `O_TRUNC`; for the latter the consumer calls
    /// this before anything can observe the file, which is all `O_TRUNC` promises.
    fn truncate<'a>(&'a self, path: &'a Path, size: u64) -> BoxFuture<'a, io::Result<()>> {
        let _ = (path, size);
        Box::pin(async { Err(io::ErrorKind::ReadOnlyFilesystem.into()) })
    }

    /// Move the entry at `from` to `to`, replacing whatever was there.
    ///
    /// No flags: libfuse-t's `rename` has no flags argument, so `RENAME_NOREPLACE`/
    /// `RENAME_EXCHANGE` could not be honoured everywhere; bindings that receive them answer
    /// `EINVAL`, as Linux does for a flag it cannot serve.
    ///
    /// Both paths belong to *this* backend; a move across a mount boundary is
    /// [`CrossesDevices`](io::ErrorKind::CrossesDevices), decided where the mount table is.
    ///
    /// Overwrite rules follow `rename(2)` (free for a local backend via `fs::rename`): a file
    /// replaces a file, a directory replaces an *empty* directory, mismatches are
    /// `EISDIR`/`ENOTDIR`/`ENOTEMPTY`, a self-rename is a no-op, and moving a directory inside
    /// itself is `EINVAL`.
    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        let _ = (from, to);
        Box::pin(async { Err(io::ErrorKind::ReadOnlyFilesystem.into()) })
    }

    /// Make what is written to `path` durable now — a guest's `fsync`.
    ///
    /// Not a "done" signal (there is none): it may arrive mid-stream, many times, and
    /// indistinguishably after the final write, so a backend must not treat it as the finished
    /// file.
    ///
    /// Defaults to `Ok`, since writes are durable on return (see *Durability*).
    fn flush<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        let _ = path;
        Box::pin(async { Ok(()) })
    }

    /// Drop whatever is being kept, so the next read asks the source again.
    ///
    /// For stores that render or cache remotely, so a reader who knows the source moved on can
    /// say so. Not for consistency a `stat` would catch; for what a store cannot check cheaply,
    /// triggered when a person asks.
    ///
    /// Infallible: a store that cannot drop something has answered by keeping it, and an error
    /// the caller cannot act on is worse than a best effort.
    fn forget<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// A shared backend is itself a backend: every call forwards to the one inside.
///
/// `?Sized` so one impl covers `Arc<dyn FileSystem>` too, which a second impl would overlap.
///
/// Consumers take a backend by value, so this lets one store serve several at once (e.g. an
/// agent via a host mount and a person over HTTP); with no opens, two consumers are just two
/// callers. Inner futures are returned untouched, so sharing adds no allocation.
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
