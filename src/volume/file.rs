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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemVolume, Workspace};

    #[test]
    fn open_routes_through_workspace_mount() {
        let mut ws = Workspace::new();
        ws.mount("data", Box::new(InMemVolume::new())).unwrap();

        // A File opened on the workspace writes into the mounted volume, seen
        // through the workspace at the same path.
        let mut f = ws
            .open("data/log", OpenOptions::new().append(true).create(true))
            .unwrap();
        f.write_all(b"a").unwrap();
        f.write_all(b"b").unwrap();
        assert_eq!(ws.read(Path::new("data/log")).unwrap(), b"ab");
    }

    #[test]
    fn seek_and_partial_write() {
        let vol = InMemVolume::new();
        let mut f = File::open(&vol, "log", OpenOptions::new().write(true).create(true)).unwrap();
        f.write_all(b"hello world").unwrap();

        // Overwrite "world" -> "rust!" via an explicit seek.
        f.seek(SeekFrom::Start(6)).unwrap();
        f.write_all(b"rust!").unwrap();
        assert_eq!(vol.read(Path::new("log")).unwrap(), b"hello rust!");

        // Read back from the start with the std toolbox.
        let mut f = File::open(&vol, "log", OpenOptions::new().read(true)).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello rust!");
    }

    #[test]
    fn append_always_writes_at_end() {
        let vol = InMemVolume::new();
        let opts = OpenOptions::new().append(true).create(true);

        let mut a = File::open(&vol, "log", opts).unwrap();
        a.write_all(b"one\n").unwrap();
        // A stale seek must not move an append write off the end.
        a.seek(SeekFrom::Start(0)).unwrap();
        a.write_all(b"two\n").unwrap();
        assert_eq!(vol.read(Path::new("log")).unwrap(), b"one\ntwo\n");
    }

    #[test]
    fn seek_from_end_and_io_copy() {
        let vol = InMemVolume::new();
        vol.write(Path::new("src"), b"0123456789").unwrap();

        let mut src = File::open(&vol, "src", OpenOptions::new().read(true)).unwrap();
        src.seek(SeekFrom::End(-3)).unwrap();
        let mut dst = File::open(&vol, "dst", OpenOptions::new().write(true).create(true)).unwrap();
        io::copy(&mut src, &mut dst).unwrap();

        assert_eq!(vol.read(Path::new("dst")).unwrap(), b"789");
    }

    #[test]
    fn open_flags() {
        let vol = InMemVolume::new();
        assert!(matches!(
            File::open(&vol, "missing", OpenOptions::new().read(true)),
            Err(CortexError::NotFound)
        ));

        File::open(&vol, "f", OpenOptions::new().create_new(true)).unwrap();
        assert!(matches!(
            File::open(&vol, "f", OpenOptions::new().create_new(true)),
            Err(CortexError::AlreadyExists)
        ));

        vol.write(Path::new("f"), b"stale").unwrap();
        File::open(&vol, "f", OpenOptions::new().write(true).truncate(true)).unwrap();
        assert_eq!(vol.read(Path::new("f")).unwrap(), b"");
    }

    #[test]
    fn seek_current_and_position() {
        let vol = InMemVolume::new();
        vol.write(Path::new("f"), b"abcdef").unwrap();
        let mut f = File::open(&vol, "f", OpenOptions::new().read(true)).unwrap();

        assert_eq!(f.seek(SeekFrom::Current(2)).unwrap(), 2);
        let mut two = [0u8; 2];
        f.read_exact(&mut two).unwrap();
        assert_eq!(&two, b"cd");
        assert_eq!(f.position(), 4);

        // Relative rewind, then read forward again.
        assert_eq!(f.seek(SeekFrom::Current(-1)).unwrap(), 3);
        let mut one = [0u8; 1];
        f.read_exact(&mut one).unwrap();
        assert_eq!(&one, b"d");
    }

    #[test]
    fn negative_seek_errors() {
        let vol = InMemVolume::new();
        vol.write(Path::new("f"), b"abc").unwrap();
        let mut f = File::open(&vol, "f", OpenOptions::new().read(true)).unwrap();

        let err = f.seek(SeekFrom::Current(-5)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(f.position(), 0); // position is unchanged on a rejected seek
    }

    #[test]
    fn read_past_eof_returns_zero() {
        let vol = InMemVolume::new();
        vol.write(Path::new("f"), b"abc").unwrap();
        let mut f = File::open(&vol, "f", OpenOptions::new().read(true)).unwrap();

        f.seek(SeekFrom::End(0)).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(f.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn create_keeps_existing_and_makes_missing() {
        let vol = InMemVolume::new();
        vol.write(Path::new("keep"), b"data").unwrap();

        // `create` (not `create_new`, not `truncate`) must not clobber contents.
        File::open(&vol, "keep", OpenOptions::new().write(true).create(true)).unwrap();
        assert_eq!(vol.read(Path::new("keep")).unwrap(), b"data");

        // ...but a missing file is created empty.
        File::open(&vol, "fresh", OpenOptions::new().write(true).create(true)).unwrap();
        assert_eq!(vol.read(Path::new("fresh")).unwrap(), b"");
    }

    #[test]
    fn directory_io_surfaces_error() {
        let vol = InMemVolume::new();
        vol.mkdir(Path::new("d")).unwrap();

        // A directory can be opened, but byte IO on it is an io::Error, not a panic.
        let mut f = File::open(&vol, "d", OpenOptions::new().read(true)).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(f.read(&mut buf).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
