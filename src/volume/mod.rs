mod file;
mod mem;
mod passthrough;

use crate::error::{Result, CortexError};
use crate::stat::{DirentKind, Stat};
use std::path::Path;

pub use file::{File, OpenOptions};
pub use mem::InMemVolume;
pub use passthrough::PassthroughVolume;

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

pub trait Mountable {
    fn list(&self, path: &Path) -> Result<Vec<Dirent>>;

    fn mkdir(&self, path: &Path) -> Result<()>;

    fn unlink(&self, path: &Path) -> Result<()>;

    fn read(&self, path: &Path) -> Result<Vec<u8>>;

    fn write(&self, path: &Path, data: &[u8]) -> Result<()>;

    /// Metadata for one entry. A [`File`] needs it to resolve `SeekFrom::End`
    /// and to find the end when appending. Default: whole-file [`read`], so it
    /// only works on files — backends override to also stat directories.
    fn stat(&self, path: &Path) -> Result<Stat> {
        let data = self.read(path)?;
        Ok(Stat::new(DirentKind::File, data.len() as u64))
    }

    /// Read up to `len` bytes at `offset`, returning fewer at EOF. Default:
    /// slice a whole-file [`read`]; backends override for true random access.
    fn read_at(&self, path: &Path, offset: u64, len: usize) -> Result<Vec<u8>> {
        let data = self.read(path)?;
        let start = (offset as usize).min(data.len());
        let end = start.saturating_add(len).min(data.len());
        Ok(data[start..end].to_vec())
    }

    /// Write `buf` at `offset`, zero-extending the file (and creating it) if
    /// needed; returns the bytes written. Default: whole-file read-modify-write.
    fn write_at(&self, path: &Path, offset: u64, buf: &[u8]) -> Result<usize> {
        let mut data = match self.read(path) {
            Ok(d) => d,
            Err(CortexError::NotFound) => Vec::new(),
            Err(e) => return Err(e),
        };
        let off = offset as usize;
        let end = off + buf.len();
        if data.len() < end {
            data.resize(end, 0);
        }
        data[off..end].copy_from_slice(buf);
        self.write(path, &data)?;
        Ok(buf.len())
    }

    /// Resize a file to `size` bytes, zero-filling any growth. Default:
    /// whole-file read-modify-write.
    fn truncate(&self, path: &Path, size: u64) -> Result<()> {
        let mut data = self.read(path).unwrap_or_default();
        data.resize(size as usize, 0);
        self.write(path, &data)
    }
}
