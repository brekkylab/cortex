//! A [`Mountable`] backend that passes every operation straight through to the
//! real local filesystem via `std::fs`.
//!
//! A `PassthroughVolume` is anchored at a `root` directory on disk. Every
//! request path is treated as relative to that root — leading `/` and `.` are
//! ignored, while `..` (and OS prefixes) are rejected so a request can never
//! escape the root.

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::{CortexError, Dirent, DirentKind, Mountable, OpenOptions, Result, Stat};

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
            // `file_type` comes from the directory entry itself (`d_type`), so
            // the kind is free. Size and timestamps are not — they would be an
            // `lstat` per entry, which a plain `ls` never asked for. So this
            // listing leaves `Dirent::stat` unset and lets the caller decide.
            let kind = if entry.file_type()?.is_dir() {
                DirentKind::Dir
            } else {
                DirentKind::File
            };
            out.push(Dirent::new(name, kind));
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
        if fs::symlink_metadata(&real)?.is_dir() {
            return Err(CortexError::IsADirectory);
        }
        fs::remove_file(&real)?;
        Ok(())
    }

    fn rmdir(&self, path: &Path) -> Result<()> {
        let real = self.real_path(path)?;
        if !fs::symlink_metadata(&real)?.is_dir() {
            return Err(CortexError::NotADirectory);
        }
        // `remove_dir` — never `remove_dir_all`. The emptiness check is the
        // kernel's own (`ENOTEMPTY`), which `From<io::Error>` maps to
        // `NotEmpty`.
        fs::remove_dir(&real)?;
        Ok(())
    }

    fn open(&self, path: &Path, options: OpenOptions) -> Result<(Self::Handle, Stat)> {
        options.validate()?;
        let real = self.real_path(path)?;

        // Hand the whole option set to the OS in one `open`, so `O_EXCL` and
        // `O_TRUNC` are as atomic here as they are for any other program. A
        // pre-flight `symlink_metadata` check would only add a race — the kernel
        // already reports `EISDIR` for a directory.
        let file = fs::OpenOptions::new()
            .read(options.read)
            .write(options.write)
            .append(options.append)
            .truncate(options.truncate)
            .create(options.create)
            .create_new(options.create_new)
            .open(&real)?;

        let meta = file.metadata()?;
        let mut stat = Stat::new(DirentKind::File, meta.len());
        stat.mtime = meta.modified().ok();
        stat.atime = meta.accessed().ok();
        stat.created = meta.created().ok();
        Ok((file, stat))
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
            .map(|e| e.name.clone())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn stat_open_read_write() {
        let base = scratch("rwlu");
        let vol = PassthroughVolume::new(&base);

        vol.mkdir(Path::new("sub")).unwrap();
        fs::write(base.join("hello.txt"), b"world").unwrap();

        let st = vol.stat(Path::new("hello.txt")).unwrap();
        assert_eq!(st.kind, DirentKind::File);
        assert_eq!(st.size, 5);
        assert_eq!(vol.stat(Path::new("sub")).unwrap().kind, DirentKind::Dir);

        // Positioned read through the open handle.
        let (handle, _) = vol.open(Path::new("hello.txt"), OpenOptions::read_write()).unwrap();
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
    fn a_listing_reports_kinds_but_not_metadata() {
        let base = scratch("listing");
        let vol = PassthroughVolume::new(&base);
        vol.mkdir(Path::new("sub")).unwrap();
        fs::write(base.join("f"), b"12345").unwrap();

        let entries = vol.list(Path::new("")).unwrap();
        let dir = entries.iter().find(|e| e.name == "sub").unwrap();
        let file = entries.iter().find(|e| e.name == "f").unwrap();

        // The kind rides along in the directory entry itself (`d_type`), so it
        // is free and always reported.
        assert_eq!(dir.kind, DirentKind::Dir);
        assert_eq!(file.kind, DirentKind::File);

        // The size is not: it would be an `lstat` per entry, which a plain `ls`
        // never asked for. A local listing therefore reports no metadata, and a
        // caller that wants it asks per entry — the opposite of an object store,
        // where the listing already contains it.
        assert!(dir.stat.is_none());
        assert!(file.stat.is_none());
        assert_eq!(vol.stat(Path::new("f")).unwrap().size, 5);

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn unlink_takes_files_and_rmdir_takes_empty_directories() {
        let base = scratch("removal");
        let vol = PassthroughVolume::new(&base);
        vol.mkdir(Path::new("dir")).unwrap();
        fs::write(base.join("dir/child"), b"x").unwrap();
        fs::write(base.join("file"), b"y").unwrap();

        assert!(matches!(
            vol.unlink(Path::new("dir")),
            Err(CortexError::IsADirectory)
        ));
        assert!(matches!(
            vol.rmdir(Path::new("file")),
            Err(CortexError::NotADirectory)
        ));
        // The old implementation reached for `remove_dir_all` here, which would
        // have taken `child` with it. Nothing on disk may disappear.
        assert!(matches!(
            vol.rmdir(Path::new("dir")),
            Err(CortexError::NotEmpty)
        ));
        assert!(base.join("dir/child").exists());

        vol.unlink(Path::new("dir/child")).unwrap();
        vol.rmdir(Path::new("dir")).unwrap();
        assert!(!base.join("dir").exists());

        assert!(matches!(
            vol.rmdir(Path::new("dir")),
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
            vol.open(Path::new("dir"), OpenOptions::read_write()),
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
            vol.open(Path::new("../secret"), OpenOptions::read_write()),
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
