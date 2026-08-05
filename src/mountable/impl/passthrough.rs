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

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        // The whole overwrite contract is the kernel's here, and `From<io::Error>`
        // already carries its answers across: measured on macOS, `fs::rename` gives
        // `EISDIR` for file-over-directory, `ENOTDIR` for the reverse, `ENOTEMPTY`
        // for a non-empty destination, `EINVAL` for a directory into its own
        // descendant, `ENOENT` for a missing source or destination parent, and
        // silently replaces file-over-file. Re-deriving any of that here could only
        // introduce disagreement with the platform.
        fs::rename(self.real_path(from)?, self.real_path(to)?)?;
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

// Tests live beside this file rather than inside it: they had grown longer than
// the implementation, so a reader opening it had to scroll past them to find the
// code. They are still a child module, so private items stay reachable.
#[cfg(test)]
#[path = "passthrough_tests.rs"]
mod tests;
