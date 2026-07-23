//! A [`Mountable`] backend that passes every operation straight through to the
//! real local filesystem via `std::fs`.
//!
//! A `PassthroughVolume` is anchored at a `root` directory on disk. Every
//! request path is treated as relative to that root — leading `/` and `.` are
//! ignored, while `..` (and OS prefixes) are rejected so a request can never
//! escape the root.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use crate::{CortexError, Dirent, DirentKind, Mountable, Result, Stat};

/// A volume backed by a real on-disk directory.
pub struct PassthroughVolume {
    root: PathBuf,
}

impl PassthroughVolume {
    /// Anchor the volume at `root` without touching the filesystem. The
    /// directory need not exist yet; operations fail later if it is missing.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        PassthroughVolume { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Map a request path to its real on-disk location under `root`. `..`,
    /// prefixes, and non-`Normal` components (other than the ignored root/`.`)
    /// are rejected so the result always stays within `root`.
    fn real_path(&self, path: &Path) -> Result<PathBuf> {
        let mut real = self.root.clone();
        for comp in path.components() {
            match comp {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => real.push(name),
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(CortexError::InvalidName);
                }
            }
        }
        Ok(real)
    }
}

impl Mountable for PassthroughVolume {
    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let real = self.real_path(path)?;
        if !fs::symlink_metadata(&real)?.is_dir() {
            return Err(CortexError::NotADirectory);
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&real)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() {
                out.push(Dirent::Dir(name));
            } else {
                out.push(Dirent::File(name));
            }
        }
        Ok(out)
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        let real = self.real_path(path)?;
        match fs::symlink_metadata(&real) {
            Ok(meta) if meta.is_dir() => Ok(()),
            Ok(_) => Err(CortexError::AlreadyExists),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&real)?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    fn unlink(&self, path: &Path) -> Result<()> {
        let real = self.real_path(path)?;
        let meta = fs::symlink_metadata(&real)?;
        if meta.is_dir() {
            fs::remove_dir_all(&real)?;
        } else {
            fs::remove_file(&real)?;
        }
        Ok(())
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        let real = self.real_path(path)?;
        if fs::symlink_metadata(&real)?.is_dir() {
            return Err(CortexError::IsADirectory);
        }
        Ok(fs::read(&real)?)
    }

    fn write(&self, path: &Path, data: &[u8]) -> Result<()> {
        let real = self.real_path(path)?;
        if let Ok(meta) = fs::symlink_metadata(&real) {
            if meta.is_dir() {
                return Err(CortexError::IsADirectory);
            }
        }
        fs::write(&real, data)?;
        Ok(())
    }

    fn stat(&self, path: &Path) -> Result<Stat> {
        let real = self.real_path(path)?;
        let meta = fs::symlink_metadata(&real)?;
        let kind = if meta.is_dir() {
            DirentKind::Dir
        } else {
            DirentKind::File
        };
        let mut stat = Stat::new(kind, meta.len());
        stat.mtime = meta.modified().ok();
        stat.atime = meta.accessed().ok();
        stat.created = meta.created().ok();
        Ok(stat)
    }

    fn read_at(&self, path: &Path, offset: u64, len: usize) -> Result<Vec<u8>> {
        let real = self.real_path(path)?;
        if fs::symlink_metadata(&real)?.is_dir() {
            return Err(CortexError::IsADirectory);
        }
        let mut file = fs::File::open(&real)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len];
        let mut read = 0;
        while read < len {
            match file.read(&mut buf[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        buf.truncate(read);
        Ok(buf)
    }

    fn write_at(&self, path: &Path, offset: u64, buf: &[u8]) -> Result<usize> {
        let real = self.real_path(path)?;
        if let Ok(meta) = fs::symlink_metadata(&real) {
            if meta.is_dir() {
                return Err(CortexError::IsADirectory);
            }
        }
        // Writing past EOF leaves a zero-filled (sparse) gap, matching the
        // zero-extend semantics of the default `write_at`.
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&real)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(buf)?;
        Ok(buf.len())
    }

    fn truncate(&self, path: &Path, size: u64) -> Result<()> {
        let real = self.real_path(path)?;
        let file = fs::OpenOptions::new().write(true).open(&real)?;
        file.set_len(size)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a unique scratch directory under the system temp dir without
    /// pulling in extra crates.
    fn scratch(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "cortex-passthrough-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(vol: &dyn Mountable, path: &str) -> Vec<String> {
        let mut names: Vec<_> = vol
            .list(Path::new(path))
            .unwrap()
            .iter()
            .map(|e| e.name().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_read_list_unlink() {
        let base = scratch("rwlu");
        let vol = PassthroughVolume::new(&base);

        vol.mkdir(Path::new("sub")).unwrap();
        vol.write(Path::new("hello.txt"), b"world").unwrap();
        vol.write(Path::new("sub/inner"), b"hi").unwrap();

        assert_eq!(vol.read(Path::new("hello.txt")).unwrap(), b"world");
        assert_eq!(vol.read(Path::new("sub/inner")).unwrap(), b"hi");
        assert_eq!(names(&vol, ""), vec!["hello.txt", "sub"]);
        assert_eq!(names(&vol, "sub"), vec!["inner"]);

        vol.unlink(Path::new("hello.txt")).unwrap();
        assert!(matches!(
            vol.read(Path::new("hello.txt")),
            Err(CortexError::NotFound)
        ));

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn kind_errors() {
        let base = scratch("kind");
        let vol = PassthroughVolume::new(&base);
        vol.mkdir(Path::new("dir")).unwrap();
        vol.write(Path::new("file"), b"x").unwrap();

        assert!(matches!(
            vol.read(Path::new("dir")),
            Err(CortexError::IsADirectory)
        ));
        assert!(matches!(
            vol.write(Path::new("dir"), b"y"),
            Err(CortexError::IsADirectory)
        ));
        assert!(matches!(
            vol.list(Path::new("file")),
            Err(CortexError::NotADirectory)
        ));
        assert!(matches!(
            vol.mkdir(Path::new("file")),
            Err(CortexError::AlreadyExists)
        ));

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn escapes_and_missing_root_rejected() {
        let base = scratch("escape");
        let vol = PassthroughVolume::new(&base);
        assert!(matches!(
            vol.read(Path::new("../secret")),
            Err(CortexError::InvalidName)
        ));
        fs::remove_dir_all(&base).unwrap();

        // `new` doesn't touch disk, so a missing root only surfaces on use.
        let missing = scratch("missing");
        fs::remove_dir_all(&missing).unwrap();
        let vol = PassthroughVolume::new(&missing);
        assert!(matches!(vol.list(Path::new("")), Err(CortexError::NotFound)));
    }
}
