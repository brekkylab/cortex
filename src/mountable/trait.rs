use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::{DirentKind, Result, Stat};

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
    pub stat: Option<Stat>,
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
}

/// How a file should be opened.
///
/// The options travel *with* the open rather than being a separate `create`,
/// because two of them are atomicity requirements only the backend can meet:
///
/// * `create_new` is `O_EXCL`. Decomposing it into "stat, then create if absent"
///   is a race, not a contract — and for a local backend (`O_CREAT|O_EXCL`) or an
///   object store (`If-None-Match: *`) the atomic form is the only one there is.
/// * `truncate` must take effect *before* any handle exists, so the metadata the
///   caller gets back already reflects the empty file. [`FileHandle::truncate`]
///   is the other, non-atomic resize; a backend must not treat one as the other.
///
/// The only meaningless combination — neither `read` nor `write` — is rejected by
/// [`validate`](Self::validate). `O_RDONLY | O_CREAT` is ordinary POSIX and stays
/// legal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenOptions {
    pub read: bool,
    pub write: bool,
    /// Writes land at the end regardless of the offset given. Mostly inert under
    /// FUSE (the kernel resolves `O_APPEND` and sends absolute offsets), but a
    /// library caller means it, so a handle must honour rather than ignore it.
    pub append: bool,
    pub truncate: bool,
    /// Create the file if it is absent. The parent directory is never created.
    pub create: bool,
    /// Create the file, failing with [`AlreadyExists`] if it is already there.
    ///
    /// [`AlreadyExists`]: crate::CortexError::AlreadyExists
    pub create_new: bool,
}

impl OpenOptions {
    pub fn read_only() -> Self {
        OpenOptions {
            read: true,
            ..Default::default()
        }
    }

    pub fn read_write() -> Self {
        OpenOptions {
            read: true,
            write: true,
            ..Default::default()
        }
    }

    /// Create a file that must not already exist, opened for reading and
    /// writing — `O_CREAT | O_EXCL | O_RDWR`, and the shape of
    /// [`File::create_new`](std::fs::File::create_new).
    ///
    /// Here because "make a new file" is the most common thing a caller wants
    /// and, without it, the only way to say it is a six-line struct update. The
    /// combination is also the one that has to be exclusive, so spelling it once
    /// keeps every caller on the atomic form.
    pub fn create_new() -> Self {
        OpenOptions {
            create_new: true,
            ..Self::read_write()
        }
    }

    /// Reject the one self-contradictory combination, so each backend spends a
    /// line calling this rather than re-deriving the rule.
    pub fn validate(&self) -> Result<()> {
        if !self.read && !self.write {
            return Err(crate::CortexError::InvalidArgument);
        }
        Ok(())
    }
}

/// A logical, path-addressed store that can be exposed as a filesystem.
///
/// The namespace/metadata plane (`stat`/`list`/`mkdir`/`unlink`) is addressed
/// by path — a FUSE `lookup`/`getattr`/`readdir` needs it *without* opening
/// anything (and a directory can't be opened at all). The data plane is reached
/// through [`open`](Self::open), which yields a stateful [`FileHandle`]: a
/// backend can do its expensive setup (auth, path resolution, a range reader,
/// a multipart upload) once and amortize it across the handle's I/O.
///
/// Uses an associated `Handle` type, so it is **not** object-safe — use
/// [`DynMountable`] where a `dyn`/`Box` is needed.
pub trait Mountable: Send + Sync {
    /// The open-file handle this backend hands out.
    type Handle: super::FileHandle;

    /// Metadata for one entry (works on files *and* directories).
    fn stat(&self, path: &Path) -> Result<Stat>;

    /// The entries directly under `path`.
    ///
    /// Each entry carries metadata only if the listing already had it; see
    /// [`Dirent::stat`].
    fn list(&self, path: &Path) -> Result<Vec<Dirent>>;

    /// Create a directory at `path`.
    fn mkdir(&self, path: &Path) -> Result<()>;

    /// Remove the *file* at `path`; a directory is rejected with
    /// [`IsADirectory`]. Use [`rmdir`] for those.
    ///
    /// Split because a filesystem never deletes recursively: `rm -rf` is
    /// decomposed by the caller into `list`, an `unlink` per file, and a final
    /// `rmdir`. A backend that quietly removed a subtree here would only ever be
    /// reached by mistake.
    ///
    /// [`IsADirectory`]: crate::CortexError::IsADirectory
    /// [`rmdir`]: Self::rmdir
    fn unlink(&self, path: &Path) -> Result<()>;

    /// Remove the *empty directory* at `path`.
    ///
    /// A file is rejected with [`NotADirectory`], and a directory that still
    /// has children with [`NotEmpty`].
    ///
    /// [`NotADirectory`]: crate::CortexError::NotADirectory
    /// [`NotEmpty`]: crate::CortexError::NotEmpty
    fn rmdir(&self, path: &Path) -> Result<()>;

    /// Open the file at `path`, returning the handle with the entry's metadata as
    /// of the open.
    ///
    /// The [`Stat`] comes back together because a FUSE `create` must answer with
    /// attributes *and* a handle in one message; a second [`stat`](Self::stat)
    /// would cost another round trip and leave a window for the entry to be
    /// replaced. See [`OpenOptions`] for which options must be atomic.
    fn open(&self, path: &Path, options: OpenOptions) -> Result<(Self::Handle, Stat)>;

    /// Move the entry at `from` to `to`, replacing whatever was there.
    ///
    /// The one operation with a default, because most backends are read-only and a
    /// store that cannot write has nothing to say here beyond "no". `ReadOnly`
    /// rather than [`Unsupported`](crate::CortexError::Unsupported), whose doc
    /// carries the reason: `EROFS` is a state userspace has a path for, `ENOSYS` is
    /// a filesystem that cannot do the operation at all.
    ///
    /// No flags. `RENAME_NOREPLACE`/`RENAME_EXCHANGE` reach two of the three
    /// bindings but libfuse-t's `rename` has no flags argument at all, so a
    /// contract carrying them could not be honoured everywhere; the bindings that
    /// receive them answer `EINVAL`, as Linux does for a flag it cannot serve.
    ///
    /// Both paths belong to *this* backend. A move that crosses a mount boundary
    /// is [`CrossDevice`](crate::CortexError::CrossDevice), decided a layer up
    /// where the mount table is visible.
    ///
    /// Overwrite rules follow `rename(2)`, which a local backend gets for free
    /// from `fs::rename`: a file replaces a file, a directory replaces an *empty*
    /// directory, and the mismatched pairs are `EISDIR`/`ENOTDIR`/`ENOTEMPTY`.
    /// Renaming a path onto itself succeeds without doing anything, and moving a
    /// directory inside itself is `EINVAL`.
    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let _ = (from, to);
        Err(crate::CortexError::ReadOnly)
    }
}

/// A shared backend is itself a backend: every call forwards to the one inside.
///
/// Needed because each consumer takes its backend *by value* and offers no way
/// back — [`PosixFs::new`](crate::PosixFs::new) has no accessor, and
/// [`Workspace::mount`](crate::Workspace::mount) boxes what it is handed. Without
/// this, a store feeds exactly one consumer, so one workspace cannot be served to
/// an agent through a host mount and to a person over HTTP at the same time.
///
/// The handle type passes straight through, so sharing costs nothing on the data
/// plane: `Arc<T>` hands out `T`'s own handles rather than erased ones.
impl<T: Mountable> Mountable for Arc<T> {
    type Handle = T::Handle;

    fn stat(&self, path: &Path) -> Result<Stat> {
        Mountable::stat(&**self, path)
    }

    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        Mountable::list(&**self, path)
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        Mountable::mkdir(&**self, path)
    }

    fn unlink(&self, path: &Path) -> Result<()> {
        Mountable::unlink(&**self, path)
    }

    fn rmdir(&self, path: &Path) -> Result<()> {
        Mountable::rmdir(&**self, path)
    }

    fn open(&self, path: &Path, options: OpenOptions) -> Result<(Self::Handle, Stat)> {
        Mountable::open(&**self, path, options)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        Mountable::rename(&**self, from, to)
    }
}

/// The object-safe face of [`Mountable`].
///
/// [`Mountable`] carries an associated `Handle` type, so `dyn Mountable` is
/// illegal — yet a heterogeneous mount table (see [`Workspace`]) needs to store
/// backends of different concrete types behind one pointer. `DynMountable`
/// erases the handle to `Box<dyn FileHandle>`: every method mirrors `Mountable`,
/// and a blanket impl makes *every* `Mountable` a `DynMountable` automatically,
/// so callers never implement it by hand.
///
/// [`Workspace`]: crate::Workspace
pub trait DynMountable: Send + Sync {
    /// See [`Mountable::stat`].
    fn stat(&self, path: &Path) -> Result<Stat>;

    /// See [`Mountable::list`].
    fn list(&self, path: &Path) -> Result<Vec<Dirent>>;

    /// See [`Mountable::mkdir`].
    fn mkdir(&self, path: &Path) -> Result<()>;

    /// See [`Mountable::unlink`].
    fn unlink(&self, path: &Path) -> Result<()>;

    /// See [`Mountable::rmdir`].
    fn rmdir(&self, path: &Path) -> Result<()>;

    /// See [`Mountable::open`], with the concrete handle boxed behind a trait
    /// object.
    fn open(&self, path: &Path, options: OpenOptions) -> Result<(Box<dyn FileHandle>, Stat)>;

    /// See [`Mountable::rename`]. No default here — the blanket impl always
    /// supplies one, forwarding to whatever the backend decided.
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
}

/// Every [`Mountable`] is a [`DynMountable`] once its handle is boxed. The
/// `'static` bound lets the erased handle become a `Box<dyn FileHandle>`.
impl<T> DynMountable for T
where
    T: Mountable,
    T::Handle: 'static,
{
    fn stat(&self, path: &Path) -> Result<Stat> {
        Mountable::stat(self, path)
    }

    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        Mountable::list(self, path)
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        Mountable::mkdir(self, path)
    }

    fn unlink(&self, path: &Path) -> Result<()> {
        Mountable::unlink(self, path)
    }

    fn rmdir(&self, path: &Path) -> Result<()> {
        Mountable::rmdir(self, path)
    }

    fn open(&self, path: &Path, options: OpenOptions) -> Result<(Box<dyn FileHandle>, Stat)> {
        let (handle, stat) = Mountable::open(self, path, options)?;
        Ok((Box::new(handle), stat))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        Mountable::rename(self, from, to)
    }
}

/// A stateful, open-file handle over some backend.
///
/// The data plane is [`FileExt`]: offset-addressed (not
/// cursor-carrying) so the handle matches how FUSE drives it — the guest kernel
/// owns the position and passes an absolute `offset` on every call, and
/// concurrent readers need no separate cursors. Backends get `read_at`/
/// `write_at` (and the whole-buffer `read_exact_at`/`write_all_at`) from
/// [`FileExt`].
///
/// This trait adds the hooks std has no portable trait for: [`truncate`] to
/// resize (FUSE `setattr`), and the two-stage write-out below. All take `&self`,
/// so they work on the `Arc`-shared handles concurrent readers hold.
///
/// [`truncate`]: Self::truncate
pub trait FileHandle: FileExt + Send + Sync {
    /// Resize the file to `size` bytes, zero-filling any growth.
    fn truncate(&self, size: u64) -> Result<()>;

    fn flush(&self) -> Result<()> {
        Ok(())
    }

    fn commit(&self) -> Result<()> {
        self.flush()
    }
}

/// The attribute changes a `setattr` asks for. Optional because the kernel sends
/// a validity mask: only the named fields are meant to move.
#[derive(Clone, Copy, Debug, Default)]
pub struct SetAttr {
    /// Resize the file. The only field this crate can actually act on, via
    /// [`FileHandle::truncate`].
    pub size: Option<u64>,
    pub mtime: Option<std::time::SystemTime>,
    pub atime: Option<std::time::SystemTime>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// A plain [`std::fs::File`] is a ready-made handle for a local passthrough
/// backend: [`FileExt`] supplies the data plane, `set_len`
/// resizes, and there is nothing buffered to flush.
impl FileHandle for std::fs::File {
    fn truncate(&self, size: u64) -> Result<()> {
        self.set_len(size)?;
        Ok(())
    }
}

/// A boxed handle is itself a handle, so it can serve as the `Handle` of an
/// erased backend (e.g. [`Workspace`](crate::Workspace), whose handle is exactly
/// `Box<dyn FileHandle>`). Every call forwards to the inner handle.
impl FileExt for Box<dyn FileHandle> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        (**self).read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        (**self).write_at(buf, offset)
    }
}

impl FileHandle for Box<dyn FileHandle> {
    fn truncate(&self, size: u64) -> Result<()> {
        (**self).truncate(size)
    }
    fn flush(&self) -> Result<()> {
        (**self).flush()
    }
    fn commit(&self) -> Result<()> {
        (**self).commit()
    }
}

/// Cross-platform positioned I/O — the data plane of a [`FileHandle`].
///
/// Both `std::os::unix::fs::FileExt` and `std::os::windows::fs::FileExt` express
/// offset-addressed reads/writes, but under different names (`read_at`/`write_at`
/// vs `seek_read`/`seek_write`), so neither can be bounded on directly in
/// portable code. This trait gives one set of names; backends implement it, and
/// [`std::fs::File`] gets a bridge to whichever std trait the target platform
/// provides (see the `impl`s below).
///
/// The whole-buffer helpers ([`read_exact_at`](Self::read_exact_at)/
/// [`write_all_at`](Self::write_all_at)) come with defaults, mirroring std.
pub trait FileExt {
    /// Read into `buf` at `offset`, returning the bytes read — fewer than
    /// `buf.len()` (possibly `0`) at EOF.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;

    /// Write `buf` at `offset`, zero-extending the file if needed; returns the
    /// bytes written.
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>;

    /// Read the exact number of bytes required to fill `buf` from `offset`.
    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.read_at(buf, offset)? {
                0 => break,
                n => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
            }
        }
        if buf.is_empty() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            ))
        }
    }

    /// Write all of `buf` starting at `offset`, erroring if a write ever
    /// reports zero bytes.
    fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.write_at(buf, offset)? {
                0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ));
                }
                n => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
            }
        }
        Ok(())
    }
}

/// On unix, forward to `pread`/`pwrite` via std's `FileExt` — these do not move
/// the file's cursor, so `Arc`-shared concurrent readers stay independent.
#[cfg(unix)]
impl FileExt for std::fs::File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(self, buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        std::os::unix::fs::FileExt::write_at(self, buf, offset)
    }
}

/// On windows the only positioned ops are `seek_read`/`seek_write`, which *do*
/// move the handle's cursor as a side effect — concurrent positioned I/O on one
/// shared `File` can race on that cursor. A windows backend that needs true
/// independent readers should open a handle per reader rather than share one.
#[cfg(windows)]
impl FileExt for std::fs::File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(self, buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_write(self, buf, offset)
    }
}

#[cfg(test)]
#[path = "trait_tests.rs"]
mod tests;
