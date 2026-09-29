//! A [`FileSystem`] store that passes every operation through to the local filesystem via
//! `std::fs`.
//!
//! A [`PassthroughFs`] is anchored at a `root` directory. Request paths are relative to it:
//! leading `/` and `.` are ignored, `..` is folded, and an OS prefix, or a `..` with nothing
//! left to fold, is rejected.
//!
//! Folding `..` confines nothing on its own, since a symlink inside the root can point out of
//! it. So a path through a link is resolved and must land under the root. Resolving *reads*
//! links rather than following them: a link out of the root cannot be traversed, and one
//! naming something not created yet is served.
//!
//! Only operations that follow a link are checked that way. `create`, `mkdir`, `unlink`,
//! `rmdir` and `rename` act on a name and never touch what it points at, so containment covers
//! only the directory holding the name; a link out of the root is still listed and removable.
//!
//! `..` is folded lexically, and has to be: a mount table routes on a normalized key and hands
//! a store the folded remainder, and [`Posix`](crate::fs::Posix) never sends one (the kernel
//! folds it against the parent inode). Any other fold would answer differently called directly
//! than through the mount table.
//!
//! This departs from the kernel behind a directory link: `dirlink/..` returns to the link's own
//! parent, not the target's. So `RESOLVE_BENEATH` matches the *escape* policy here but not the
//! `..` handling, since that flag resolves in full before judging where it landed.
//!
//! The check and the operation are separate calls, so a link swapped between them is not
//! caught. No binding implements `symlink`, so only another process on the same tree could.
//!
//! # A descriptor per call
//!
//! Every read and write opens the file, acts, and closes it: a store is addressed by path (see
//! *No opens, only paths* on [`FileSystem`]), so no descriptor outlives its call.
//!
//! That is three syscalls instead of one, and a path walk each time: noise next to a request
//! that crossed a virtio-fs ring or FUSE channel. The real cost is the POSIX guarantee a descriptor carries:
//! a read after the name is gone answers `ENOENT` where an open file would keep working.
//! Restoring that belongs to the layer holding descriptors
//! ([`Posix::unlink_child`](crate::fs::Posix)).

use std::{
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

use crate::{
    BoxFuture,
    fs::{Dirent, DirentKind, FileSystem, Stat},
};

/// Links one request may resolve through before the walk calls it a cycle. Links are read
/// rather than followed by the kernel, so its `MAXSYMLINKS` (32 on macOS and the BSDs, 40 on
/// Linux) does not apply.
const MAX_LINK_HOPS: u32 = 32;

/// A real on-disk directory, served as-is through `std::fs` under `root`.
pub struct PassthroughFs {
    root: PathBuf,

    /// `root` resolved, for containment checks: on macOS `/tmp` *is* `/private/tmp`, so the
    /// unresolved spelling would put every ordinary path outside the root.
    ///
    /// Filled on first use so [`new`](Self::new) touches no disk; kept once built, so a root
    /// replaced afterwards is measured against where it was.
    canonical_root: OnceLock<PathBuf>,
}

impl PassthroughFs {
    /// Anchor the store at `root` without touching the filesystem. The directory need not
    /// exist yet; operations fail later if it is missing.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        PassthroughFs {
            root: root.into(),
            canonical_root: OnceLock::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn canonical_root(&self) -> io::Result<&Path> {
        if let Some(root) = self.canonical_root.get() {
            return Ok(root);
        }
        let built = self.root.canonicalize()?;
        Ok(self.canonical_root.get_or_init(|| built))
    }

    /// Where `real` lands once links are resolved, or [`NotFound`](io::ErrorKind::NotFound) if
    /// that is outside the root.
    ///
    /// Links are *read*, not followed: `canonicalize` fails unless every component exists,
    /// yet a link naming something not created yet can point somewhere contained.
    ///
    /// `NotFound` rather than `InvalidFilename` (for malformed requests): the request is well
    /// formed, and `find` and `rsync` skip `ENOENT` but surface `EINVAL`.
    fn resolve_within_root(&self, real: &Path) -> io::Result<PathBuf> {
        let root = self.canonical_root()?;
        // Reversed, so `pop` walks left to right and a link's target can be pushed back on.
        let mut pending: Vec<OsString> = real
            .strip_prefix(&self.root)
            .map_err(|_| io::Error::from(io::ErrorKind::NotFound))?
            .components()
            .rev()
            .map(|comp| comp.as_os_str().to_os_string())
            .collect();
        let mut resolved = root.to_path_buf();
        let mut hops = 0u32;
        while let Some(name) = pending.pop() {
            if name == "." {
                continue;
            }
            if name == ".." {
                // Against what is already *resolved*, as the kernel does: a `..` after a link
                // climbs from the target's parent, not the link's.
                resolved.pop();
                continue;
            }
            resolved.push(&name);
            let Ok(target) = fs::read_link(&resolved) else {
                continue; // not a link, or not there — nothing to resolve either way
            };
            hops += 1;
            if hops > MAX_LINK_HOPS {
                // A cycle names nothing servable, which `NotFound` means throughout here.
                return Err(io::ErrorKind::NotFound.into());
            }
            resolved.pop();
            // An absolute target replaces what is resolved so far: `PathBuf::push` does that
            // for a leading `RootDir`.
            pending.extend(
                target
                    .components()
                    .rev()
                    .map(|comp| comp.as_os_str().to_os_string()),
            );
        }
        if !resolved.starts_with(root) {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(resolved)
    }

    /// Fold a request onto `root`: `.` dropped, `..` applied, an OS prefix or a climb past the
    /// root refused. Containment is left to callers, which check different things.
    fn fold(&self, path: &Path) -> io::Result<Folded> {
        let mut folded = Folded {
            real: self.root.clone(),
            depth: 0,
            through_a_link: false,
        };
        for comp in path.components() {
            match comp {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    folded.real.push(name);
                    folded.depth += 1;
                    // Checked here, allocation-free, so a link-free path skips the resolving
                    // walk. A `..` that folds a link away leaves this set: a wasted resolve,
                    // never a missed one.
                    folded.through_a_link =
                        folded.through_a_link || fs::read_link(&folded.real).is_ok();
                }
                // Folded, not refused: `a/b/../c` names something in the root, the same fold
                // a mount table applies a layer up. Only a climb past the root is refused.
                Component::ParentDir => {
                    if folded.depth == 0 {
                        return Err(io::ErrorKind::InvalidFilename.into());
                    }
                    folded.real.pop();
                    folded.depth -= 1;
                }
                Component::Prefix(_) => return Err(io::ErrorKind::InvalidFilename.into()),
            }
        }
        Ok(folded)
    }

    /// Map a request path under `root`, refusing one that would leave it *through* a link.
    /// For operations that follow links: `stat`, `list`, and the data plane.
    ///
    /// Returns the *unresolved* path, so the OS acts on the name the caller asked for.
    fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
        let folded = self.fold(path)?;
        // Skip the root itself: resolving needs [`canonical_root`](Self::canonical_root),
        // which would fail before the store could `mkdir` its own root.
        if folded.through_a_link && folded.depth > 0 {
            self.resolve_within_root(&folded.real)?;
        }
        Ok(folded.real)
    }

    /// Map a request path under `root` for operations on a *name* rather than what it points
    /// at: `create`, `mkdir`, `unlink`, `rmdir`, `rename`. Their `std::fs` calls never follow a
    /// trailing link, so containment covers only the parent, which keeps a link out of the
    /// root removable.
    fn entry_path(&self, path: &Path) -> io::Result<PathBuf> {
        let folded = self.fold(path)?;
        // `depth > 1`: one component down, the parent *is* the root, contained by definition
        // and not resolvable if it does not exist yet.
        if folded.through_a_link && folded.depth > 1 {
            let parent = folded.real.parent().expect("depth > 1 leaves a parent");
            self.resolve_within_root(parent)?;
        }
        Ok(folded.real)
    }

    /// Open the file at `path` for the data plane.
    ///
    /// Never creates: only [`create`](FileSystem::create) makes a name, so a write to a name
    /// that went away errors rather than resurrecting it.
    fn open(&self, path: &Path, writable: bool) -> io::Result<fs::File> {
        let real = self.real_path(path)?;
        fs::OpenOptions::new()
            .read(!writable)
            .write(writable)
            .open(real)
    }

    fn stat_of(meta: &fs::Metadata) -> Stat {
        let kind = if meta.is_dir() {
            DirentKind::Dir
        } else {
            DirentKind::File
        };
        let mut stat = Stat::new(kind, meta.len());
        stat.mtime = meta.modified().ok();
        stat.atime = meta.accessed().ok();
        stat.created = meta.created().ok();
        stat
    }
}

/// A request folded onto the root, before containment is asked about.
struct Folded {
    real: PathBuf,
    /// Components below the root, so the root itself stays recognisable.
    depth: usize,
    /// Whether a link lies anywhere on the way.
    through_a_link: bool,
}

impl FileSystem for PassthroughFs {
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            let real = self.real_path(path)?;
            // Follows links, per POSIX, so the size matches what a read returns: an `lstat`
            // gives the link string's length, and a kernel believing it truncates the read.
            Ok(Self::stat_of(&fs::metadata(&real)?))
        })
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        Box::pin(async move {
            let real = self.real_path(path)?;
            // Follows links, so a link to a directory lists as one.
            if !fs::metadata(&real)?.is_dir() {
                return Err(io::ErrorKind::NotADirectory.into());
            }
            let mut out = Vec::new();
            for entry in fs::read_dir(&real)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                // The kind is free from `d_type`; size and timestamps would cost an `lstat`
                // per entry a plain `ls` never asked for, so `Dirent::stat` stays unset.
                //
                // Links are the exception: `d_type` says DT_LNK, never DT_DIR, so only they
                // pay to ask the target.
                let file_type = entry.file_type()?;
                let kind = if file_type.is_symlink() {
                    // Only a target inside the root is described; one outside, or a dangling
                    // link, is `File` since `DirentKind` has no `Symlink`. Still listed:
                    // `unlink` can remove it, so omitting it would be wrong.
                    match self.resolve_within_root(&entry.path()) {
                        Ok(target) if fs::metadata(&target).is_ok_and(|meta| meta.is_dir()) => {
                            DirentKind::Dir
                        }
                        _ => DirentKind::File,
                    }
                } else if file_type.is_dir() {
                    DirentKind::Dir
                } else {
                    DirentKind::File
                };
                out.push(Dirent::new(name, kind));
            }
            // `read_dir` order is a hash order on APFS and ext4, and `readdir` resumes by
            // position, so the order must be stable across calls.
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
    }

    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move { pread(&self.open(path, false)?, buf, offset) })
    }

    /// `O_CREAT | O_EXCL` in one OS call, so exclusivity is the kernel's; a pre-flight check
    /// for the name would race.
    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            let real = self.entry_path(path)?;
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(real)?;
            Ok(Self::stat_of(&file.metadata()?))
        })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            let real = self.entry_path(path)?;
            // `create_dir` is already exclusive: `EEXIST` for any taken name.
            fs::create_dir(&real)?;
            Ok(Self::stat_of(&fs::symlink_metadata(&real)?))
        })
    }

    fn unlink<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let real = self.entry_path(path)?;
            if fs::symlink_metadata(&real)?.is_dir() {
                return Err(io::ErrorKind::IsADirectory.into());
            }
            fs::remove_file(&real)?;
            Ok(())
        })
    }

    fn rmdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let real = self.entry_path(path)?;
            if !fs::symlink_metadata(&real)?.is_dir() {
                return Err(io::ErrorKind::NotADirectory.into());
            }
            // Never `remove_dir_all`: the kernel's `ENOTEMPTY` arrives as `DirectoryNotEmpty`.
            fs::remove_dir(&real)?;
            Ok(())
        })
    }

    fn write_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move { pwrite(&self.open(path, true)?, buf, offset) })
    }

    fn truncate<'a>(&'a self, path: &'a Path, size: u64) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move { self.open(path, true)?.set_len(size) })
    }

    /// The overwrite contract and its errors are the kernel's, so nothing here can disagree
    /// with the platform. On macOS `fs::rename` gives `EISDIR` for file-over-directory,
    /// `ENOTDIR` for the reverse, `ENOTEMPTY` for a non-empty destination, `EINVAL` for a
    /// directory into its own descendant, `ENOENT` for a missing source or destination parent,
    /// and silently replaces file-over-file.
    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move { fs::rename(self.entry_path(from)?, self.entry_path(to)?) })
    }

    /// Bytes are already in the page cache when `write_at` returns; this asks the kernel to
    /// put them on the device.
    fn flush<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move { self.open(path, true)?.sync_all() })
    }
}

/// Positioned I/O on a [`fs::File`], under one name.
///
/// Unix names these `read_at`/`write_at` and Windows `seek_read`/`seek_write`. Windows' move
/// the cursor where `pread`/`pwrite` do not, which is harmless: each descriptor is one call's
/// own.
#[cfg(unix)]
fn pread(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

/// See [`pread`].
#[cfg(unix)]
fn pwrite(file: &fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

/// See [`pread`].
#[cfg(windows)]
fn pread(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

/// See [`pread`].
#[cfg(windows)]
fn pwrite(file: &fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}
