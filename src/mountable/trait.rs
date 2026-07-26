use std::io;
use std::path::Path;

use crate::{Result, Stat};

/// One entry in a directory listing: its name plus whether it is a file or a
/// subdirectory. (Full metadata is fetched separately via
/// [`Mountable::stat`].)
pub enum Dirent {
    Dir(String),
    File(String),
}

impl Dirent {
    /// The entry's name, regardless of whether it is a directory or a file.
    pub fn name(&self) -> &str {
        match self {
            Dirent::Dir(name) | Dirent::File(name) => name,
        }
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
    fn list(&self, path: &Path) -> Result<Vec<Dirent>>;

    /// Create a directory at `path`.
    fn mkdir(&self, path: &Path) -> Result<()>;

    /// Remove the entry at `path`.
    fn unlink(&self, path: &Path) -> Result<()>;

    /// Open the file at `path` for I/O.
    fn open(&self, path: &Path) -> Result<Self::Handle>;
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
/// This trait adds the two hooks std has no portable trait for: [`truncate`] to
/// resize (FUSE `setattr`) and [`flush`] to commit buffered writes (e.g. finish
/// an S3 multipart upload). `flush` is `&self`, not [`io::Write::flush`], so it
/// works on the `Arc`-shared handles concurrent readers hold.
///
/// [`truncate`]: Self::truncate
/// [`flush`]: Self::flush
/// [`io::Write::flush`]: std::io::Write::flush
pub trait FileHandle: FileExt + Send + Sync {
    /// Resize the file to `size` bytes, zero-filling any growth.
    fn truncate(&self, size: u64) -> Result<()>;

    /// Commit any buffered writes (e.g. complete an S3 multipart upload).
    /// Default: nothing buffered, so a no-op.
    fn flush(&self) -> Result<()> {
        Ok(())
    }
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
