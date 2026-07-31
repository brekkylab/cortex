//! A [`Mountable`] backend that passes every operation straight through to the
//! real local filesystem via `std::fs`.
//!
//! A `PassthroughVolume` is anchored at a `root` directory on disk. Every
//! request path is treated as relative to that root — leading `/` and `.` are
//! ignored, while `..` (and OS prefixes) are rejected so a request can never
//! escape the root.

use std::fs;
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
    type Handle = fs::File;

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

    fn open(&self, path: &Path) -> Result<Self::Handle> {
        let real = self.real_path(path)?;
        if fs::symlink_metadata(&real)?.is_dir() {
            return Err(CortexError::IsADirectory);
        }
        // Open for positioned reads and writes. Creation of new files is a
        // separate concern (a future FUSE `create`), so a missing path surfaces
        // as `NotFound` rather than being created here.
        Ok(fs::OpenOptions::new().read(true).write(true).open(&real)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileExt, FileHandle};

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

    fn names(vol: &dyn Mountable<Handle = fs::File>, path: &str) -> Vec<String> {
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
    fn stat_open_read_write() {
        let base = scratch("rwlu");
        let vol = PassthroughVolume::new(&base);

        vol.mkdir(Path::new("sub")).unwrap();
        // Files are created out-of-band (the volume has no `create` yet), then
        // driven through its handle.
        fs::write(base.join("hello.txt"), b"world").unwrap();

        let st = vol.stat(Path::new("hello.txt")).unwrap();
        assert_eq!(st.kind, DirentKind::File);
        assert_eq!(st.size, 5);
        assert_eq!(vol.stat(Path::new("sub")).unwrap().kind, DirentKind::Dir);

        // Positioned read through the open handle.
        let handle = vol.open(Path::new("hello.txt")).unwrap();
        let mut buf = [0u8; 5];
        handle.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"world");

        // Positioned write, then truncate, both observable on disk.
        handle.write_all_at(b"HELLO", 0).unwrap();
        handle.truncate(3).unwrap();
        assert_eq!(fs::read(base.join("hello.txt")).unwrap(), b"HEL");

        assert_eq!(names(&vol, ""), vec!["hello.txt", "sub"]);

        vol.unlink(Path::new("hello.txt")).unwrap();
        assert!(matches!(
            vol.stat(Path::new("hello.txt")),
            Err(CortexError::NotFound)
        ));

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn kind_errors() {
        let base = scratch("kind");
        let vol = PassthroughVolume::new(&base);
        vol.mkdir(Path::new("dir")).unwrap();
        fs::write(base.join("file"), b"x").unwrap();

        assert!(matches!(
            vol.open(Path::new("dir")),
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
            vol.open(Path::new("../secret")),
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
