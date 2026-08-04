//! A [`Mountable`] backend that passes every operation straight through to the
//! real local filesystem via `std::fs`.
//!
//! A `PassthroughVolume` is anchored at a `root` directory on disk. Request
//! paths are relative to it: leading `/` and `.` are ignored, `..` and OS
//! prefixes rejected.
//!
//! Rejecting `..` confines nothing on its own — a symlink inside the root can
//! point out of it. So a path with a link in it is resolved and must land under
//! the root; one without cannot leave and is not resolved. A link that escapes,
//! or dangles, is refused and left out of listings.
//!
//! The check and the operation are separate calls, so a link swapped between
//! them is not caught. No binding implements `symlink`, so nothing reachable
//! through this crate can do that; another process on the same tree could.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use crate::{CortexError, Dirent, DirentKind, Mountable, OpenOptions, Result, Stat};

/// A volume backed by a real on-disk directory.
pub struct PassthroughVolume {
    root: PathBuf,

    /// `root` resolved, which is what containment compares against — on macOS
    /// `/tmp` *is* `/private/tmp`, so the unresolved spelling would put every
    /// ordinary path outside the root.
    ///
    /// Filled on first use, so [`new`](Self::new) stays offline. Kept once
    /// built, so a root replaced afterwards is measured against where it was.
    canonical_root: OnceLock<PathBuf>,
}

impl PassthroughVolume {
    /// Anchor the volume at `root` without touching the filesystem. The
    /// directory need not exist yet; operations fail later if it is missing.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        PassthroughVolume {
            root: root.into(),
            canonical_root: OnceLock::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn canonical_root(&self) -> Result<&Path> {
        if let Some(root) = self.canonical_root.get() {
            return Ok(root);
        }
        let built = self.root.canonicalize()?;
        Ok(self.canonical_root.get_or_init(|| built))
    }

    /// Where `real` lands once links are resolved, or
    /// [`NotFound`](CortexError::NotFound) if that is outside the root.
    ///
    /// `NotFound` rather than `InvalidName`, which is for a malformed request:
    /// this one is well formed, and `list` already omits the name, so the error
    /// says what the omission does. `find` and `rsync` skip `ENOENT` and
    /// surface `EINVAL`.
    fn resolve_within_root(&self, real: &Path) -> Result<PathBuf> {
        let target = real.canonicalize()?;
        if !target.starts_with(self.canonical_root()?) {
            return Err(CortexError::NotFound);
        }
        Ok(target)
    }

    /// Map a request path to its location under `root`, refusing what would
    /// leave it. Returns the *unresolved* path: `open` and `unlink` act on the
    /// name the caller asked for, links and all.
    fn real_path(&self, path: &Path) -> Result<PathBuf> {
        // Only a link can take one of these paths out of the root, and
        // resolving costs ~10µs against ~1µs for an `lstat` (macOS `realpath`
        // opens the file), so the walk looks for one and resolves only if found.
        let mut real = self.root.clone();
        let mut through_a_link = false;
        for comp in path.components() {
            match comp {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => real.push(name),
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(CortexError::InvalidName);
                }
            }
            through_a_link = through_a_link
                || fs::symlink_metadata(&real).is_ok_and(|meta| meta.file_type().is_symlink());
        }
        if through_a_link {
            // `symlink_metadata`, never `exists()`: that follows links, so a
            // link to nowhere answers false and gets judged by its parent —
            // after which a creating `open` writes on the far side of it.
            match fs::symlink_metadata(&real) {
                Ok(_) => self.resolve_within_root(&real)?,
                // Nothing of that name yet, reached through a link. The parent
                // decides, or a create POSIX allows would be refused.
                Err(_) => self.resolve_within_root(real.parent().unwrap_or(&real))?,
            };
        }
        Ok(real)
    }
}

impl Mountable for PassthroughVolume {
    type Handle = fs::File;

    fn stat(&self, path: &Path) -> Result<Stat> {
        let real = self.real_path(path)?;
        // Follows, where `unlink` below does not — POSIX, and it is what makes
        // the reported size the one `open` returns. An `lstat` gives the link
        // string's length, and a kernel that believes it truncates the read.
        let meta = fs::metadata(&real)?;
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
        // Follows, like `stat`, which has just called such a link a directory.
        if !fs::metadata(&real)?.is_dir() {
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
            //
            // Links are the exception: `d_type` answers DT_LNK and never
            // DT_DIR, so their kind has to come from the target, and only they
            // pay for it.
            let file_type = entry.file_type()?;
            let kind = if file_type.is_symlink() {
                let Ok(target) = self.resolve_within_root(&entry.path()) else {
                    continue; // escapes the root, or dangles
                };
                if fs::metadata(&target)?.is_dir() {
                    DirentKind::Dir
                } else {
                    DirentKind::File
                }
            } else if file_type.is_dir() {
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
