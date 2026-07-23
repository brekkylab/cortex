//! [`File`]: a stateful, cursor-carrying handle over a [`Mountable`].
//!
//! The [`Mountable`] data plane (`read_at`/`write_at`) is stateless — every call
//! names its own offset, matching how a virtio-fs / FUSE server is driven (the
//! guest kernel owns the file position). `File` is the opposite end: an
//! in-process handle that owns a cursor, so a consumer such as an
//! [`Executable`](crate::Executable) can read/write/seek/append like it would
//! with [`std::fs::File`]. It borrows the volume for its lifetime and forwards
//! each operation to the stateless `*_at` ops at the current cursor.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Result, CortexError};
use crate::volume::Mountable;

/// How a [`File`] is opened. Builder-style, mirroring [`std::fs::OpenOptions`]:
/// `OpenOptions::new().append(true).create(true)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(mut self, yes: bool) -> Self {
        self.read = yes;
        self
    }

    pub fn write(mut self, yes: bool) -> Self {
        self.write = yes;
        self
    }

    /// Every write goes to the current end of the file, regardless of the seek
    /// position.
    pub fn append(mut self, yes: bool) -> Self {
        self.append = yes;
        self
    }

    /// Truncate the file to zero length on open.
    pub fn truncate(mut self, yes: bool) -> Self {
        self.truncate = yes;
        self
    }

    /// Create the file if it does not exist.
    pub fn create(mut self, yes: bool) -> Self {
        self.create = yes;
        self
    }

    /// Create the file, failing if it already exists.
    pub fn create_new(mut self, yes: bool) -> Self {
        self.create_new = yes;
        self
    }
}

/// An open file handle with its own read/write cursor.
///
/// Construct one with [`File::open`] or [`Workspace::open`](crate::Workspace::open).
/// It implements [`Read`], [`Write`], and [`Seek`], so the whole `std::io`
/// toolbox (`io::copy`, `BufReader`, `read_to_string`, …) works against a
/// [`Mountable`].
pub struct File<'a> {
    vol: &'a dyn Mountable,
    path: PathBuf,
    pos: u64,
    append: bool,
}

impl<'a> File<'a> {
    /// Open `path` on `vol` per `opts`. Applies `create`/`create_new`/`truncate`
    /// up front; the cursor starts at 0 (appends still go to the end).
    pub fn open(vol: &'a dyn Mountable, path: impl AsRef<Path>, opts: OpenOptions) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let exists = vol.stat(&path).is_ok();

        if opts.create_new {
            if exists {
                return Err(CortexError::AlreadyExists);
            }
            vol.write(&path, b"")?;
        } else if !exists {
            if opts.create {
                vol.write(&path, b"")?;
            } else {
                return Err(CortexError::NotFound);
            }
        }

        if opts.truncate {
            vol.truncate(&path, 0)?;
        }

        Ok(File {
            vol,
            path,
            pos: 0,
            append: opts.append,
        })
    }

    /// The current cursor position.
    pub fn position(&self) -> u64 {
        self.pos
    }
}

impl Read for File<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let data = self.vol.read_at(&self.path, self.pos, buf.len())?;
        let n = data.len();
        buf[..n].copy_from_slice(&data);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for File<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.append {
            self.pos = self.vol.stat(&self.path)?.size;
        }
        let n = self.vol.write_at(&self.path, self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Every `write_at` commits, so there is nothing buffered to flush.
        Ok(())
    }
}

impl Seek for File<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let base_and_delta = match pos {
            SeekFrom::Start(off) => {
                self.pos = off;
                return Ok(self.pos);
            }
            SeekFrom::Current(delta) => (self.pos, delta),
            SeekFrom::End(delta) => (self.vol.stat(&self.path)?.size, delta),
        };
        let (base, delta) = base_and_delta;
        let new = (base as i128) + (delta as i128);
        if new < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to a negative position",
            ));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}
