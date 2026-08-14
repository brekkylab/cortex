//! A [`FileSystem`] store that passes every operation straight through to the real local
//! filesystem via `std::fs`.
//!
//! A [`PassthroughFs`] is anchored at a `root` directory on disk. Request paths are
//! relative to it: leading `/` and `.` are ignored, `..` is folded, and an OS prefix — or a
//! `..` with nothing left to fold — is rejected.
//!
//! Folding `..` confines nothing on its own — a symlink inside the root can point out of it.
//! So one walk resolves what it finds and what it arrives at must be under the root; a path
//! with no link in it cannot leave, and the same walk finds nothing to resolve. Resolving
//! *reads* links rather than following them, which keeps the question to where a link points:
//! one out of the root cannot be traversed, one naming something not created yet is served.
//!
//! Only an operation that follows a link has to ask. `create`, `mkdir`, `unlink`, `rmdir` and
//! `rename` act on a name in the directory holding it and never touch what it points at, so
//! for them that directory is what containment is about — a link out of the root is still
//! listed, and can still be taken away.
//!
//! `..` is folded lexically, and has to be: a mount table routes on a normalized key and hands
//! a store the folded remainder, and [`Posix`](crate::fs::Posix) never sends one, the kernel
//! folding it against the parent inode first. Folding some other way here would answer one way
//! called directly and another through the mount table.
//!
//! It does part ways with the kernel behind a directory link — `dirlink/..` returns to the
//! link's own parent, not the target's — so `RESOLVE_BENEATH` is the *escape* policy here but
//! not the `..`, that flag resolving in full before judging where it landed.
//!
//! The check and the operation are separate calls, so a link swapped between them is not
//! caught. No binding implements `symlink`, so nothing reachable through this crate can do
//! that; another process on the same tree could.
//!
//! # A descriptor per call
//!
//! Every read and write opens the file, acts, and closes it. There is no open to hang one on:
//! a store is addressed by path (see *No opens, only paths* on [`FileSystem`]), so the
//! descriptor cannot outlive the call that made it.
//!
//! Three syscalls where a held descriptor would need one, and the path walked again each time.
//! Against a request that crossed a virtio-fs ring or a FUSE channel to get here, that is
//! noise. What it does cost is the one POSIX guarantee a descriptor carries: a read after the
//! name is gone answers `ENOENT` where an open file would have kept working. Restoring that
//! belongs to the layer holding the descriptors, not here — see
//! [`Posix::unlink_child`](crate::fs::Posix).

use std::{
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

use crate::BoxFuture;
use crate::fs::{Dirent, DirentKind, FileSystem, Stat};

/// The links one request may resolve through before the walk calls it a cycle. Reading links
/// rather than having the kernel follow them moves its ceiling here: `MAXSYMLINKS` is 32 on
/// macOS and the BSDs, 40 on Linux.
const MAX_LINK_HOPS: u32 = 32;

/// A real on-disk directory, served as-is.
///
/// The name is the mapping: every request passes through to `std::fs` under `root`.
pub struct PassthroughFs {
    root: PathBuf,

    /// `root` resolved, which is what containment compares against — on macOS `/tmp` *is*
    /// `/private/tmp`, so the unresolved spelling would put every ordinary path outside the
    /// root.
    ///
    /// Filled on first use, so [`new`](Self::new) stays offline. Kept once built, so a root
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
    /// Links are *read*, not followed. `canonicalize` answers the same question, but only for a
    /// path whose every component exists — and a link naming something not created yet points
    /// somewhere perfectly contained. Reading the link says where without asking whether.
    ///
    /// `NotFound` rather than `InvalidFilename`, which is for a malformed request: this one is
    /// well formed, so the error says what a caller holding that name will hear from every
    /// other operation. `find` and `rsync` skip `ENOENT` and surface `EINVAL`.
    fn resolve_within_root(&self, real: &Path) -> io::Result<PathBuf> {
        let root = self.canonical_root()?;
        // Reversed, so `pop` walks left to right and a link's own target can go back on for
        // the walk to continue through it.
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
                // Against what is already *resolved*, which is where the kernel applies it
                // too: a `..` after a link climbs from the target's parent, not the link's.
                resolved.pop();
                continue;
            }
            resolved.push(&name);
            let Ok(target) = fs::read_link(&resolved) else {
                continue; // not a link, or not there — nothing to resolve either way
            };
            hops += 1;
            if hops > MAX_LINK_HOPS {
                // A cycle names nothing this store can serve, which is what `NotFound` says
                // for every other unservable name here.
                return Err(io::ErrorKind::NotFound.into());
            }
            resolved.pop();
            // An absolute target replaces what is resolved so far, which `PathBuf::push` does
            // on its own for a leading `RootDir`.
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
    /// root refused. Containment is not asked about here — the two callers below ask it of
    /// different things.
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
                    // Asked here, where it costs no allocation, so a path with no link in it
                    // never reaches the resolving walk — which builds a stack it would have
                    // nothing to put on. A `..` that folds a link away leaves this set: a
                    // wasted resolve, never a missed one.
                    folded.through_a_link =
                        folded.through_a_link || fs::read_link(&folded.real).is_ok();
                }
                // Folded, not refused: `a/b/../c` names something in the root like any other
                // request, the same fold a mount table applies a layer up — spelled again, a
                // store reaching up into that layer inverting the layering. Nothing left to
                // fold is a climb past the root, which is what the refusal is for.
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

    /// Map a request path to its location under `root`, refusing one that would leave it
    /// *through* a link. For the operations that follow one: `stat`, `list`, and the data
    /// plane.
    ///
    /// Returns the *unresolved* path, so the OS acts on the name the caller asked for, links
    /// and all.
    fn real_path(&self, path: &Path) -> io::Result<PathBuf> {
        let folded = self.fold(path)?;
        // `depth` guards the root itself, which a resolve cannot be asked about: it wants
        // [`canonical_root`](Self::canonical_root) first, and that would cost a store the
        // ability to `mkdir` the directory it was anchored at.
        if folded.through_a_link && folded.depth > 0 {
            self.resolve_within_root(&folded.real)?;
        }
        Ok(folded.real)
    }

    /// The same, for the operations that act on a *name* rather than on what it points at:
    /// `create`, `mkdir`, `unlink`, `rmdir`, `rename`. None of `create_new`, `create_dir`,
    /// `remove_file`, `remove_dir` or `rename` follows a trailing link, so the directory
    /// holding the name is all they can reach and all containment has to cover — which is what
    /// keeps a link out of the root removable.
    fn entry_path(&self, path: &Path) -> io::Result<PathBuf> {
        let folded = self.fold(path)?;
        // `depth > 1`, not `> 0`: one component down the parent *is* the root, contained by
        // definition and not yet resolvable if it does not exist.
        if folded.through_a_link && folded.depth > 1 {
            let parent = folded.real.parent().expect("depth > 1 leaves a parent");
            self.resolve_within_root(parent)?;
        }
        Ok(folded.real)
    }

    /// Open the file at `path` for the data plane, in the mode `writable` asks for.
    ///
    /// Never creates: [`create`](FileSystem::create) is the one thing that makes a name, so a
    /// write to a name that went away is an error rather than a resurrection.
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
            // Follows, where `unlink` below does not — POSIX, and it is what makes the
            // reported size the one a read returns. An `lstat` gives the link string's length,
            // and a kernel that believes it truncates the read.
            Ok(Self::stat_of(&fs::metadata(&real)?))
        })
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        Box::pin(async move {
            let real = self.real_path(path)?;
            // Follows, like `stat`, which has just called such a link a directory.
            if !fs::metadata(&real)?.is_dir() {
                return Err(io::ErrorKind::NotADirectory.into());
            }
            let mut out = Vec::new();
            for entry in fs::read_dir(&real)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                // `file_type` comes from the directory entry itself (`d_type`), so the kind is
                // free. Size and timestamps are not — they would be an `lstat` per entry,
                // which a plain `ls` never asked for. So this listing leaves `Dirent::stat`
                // unset and lets the caller decide.
                //
                // Links are the exception: `d_type` answers DT_LNK and never DT_DIR, so their
                // kind has to come from the target, and only they pay for it.
                let file_type = entry.file_type()?;
                let kind = if file_type.is_symlink() {
                    // Only a target inside the root can be asked what it is; one outside is
                    // not this store's to describe. `File` is what is left either way —
                    // `DirentKind` has no `Symlink` — and it is what a link with nothing on
                    // the end of it gets too.
                    //
                    // Reported either way: omitting the name would claim it is not there,
                    // which `unlink` removing it contradicts.
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
            // `read_dir` gives the directory's own order — a hash order on APFS and ext4 —
            // and a `readdir` resumes by position, so it has to be the same on the next call.
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

    /// `O_CREAT | O_EXCL`, in one call to the OS, so exclusivity is the kernel's own — a
    /// pre-flight check for the name would be a race, and this is the one operation whose
    /// whole contract is that there is no window.
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
            // `create_dir` is already exclusive — `EEXIST` for a taken name, whatever is under
            // it — so there is nothing to check first and nothing to re-derive.
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
            // `remove_dir` — never `remove_dir_all`. The emptiness check is the kernel's own
            // (`ENOTEMPTY`), which arrives here as `DirectoryNotEmpty`.
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

    /// The whole overwrite contract is the kernel's here, and its errors travel up as
    /// themselves: measured on macOS, `fs::rename` gives `EISDIR` for file-over-directory,
    /// `ENOTDIR` for the reverse, `ENOTEMPTY` for a non-empty destination, `EINVAL` for a
    /// directory into its own descendant, `ENOENT` for a missing source or destination parent,
    /// and silently replaces file-over-file. Re-deriving any of that here could only introduce
    /// disagreement with the platform.
    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move { fs::rename(self.entry_path(from)?, self.entry_path(to)?) })
    }

    /// A durability barrier the OS has a call for, and the one place this store does more than
    /// pass a path through: the bytes are already in the page cache when `write_at` returns, so
    /// what is left is asking the kernel to put them on the device.
    fn flush<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move { self.open(path, true)?.sync_all() })
    }
}

/// Positioned I/O on a [`fs::File`], under one name.
///
/// Both `std::os::unix::fs::FileExt` and `std::os::windows::fs::FileExt` express
/// offset-addressed reads and writes, but under different names (`read_at`/`write_at` against
/// `seek_read`/`seek_write`), so neither can be called from portable code.
///
/// The descriptor is this call's own either way, so nothing here shares a cursor — which is the
/// difference that would otherwise matter, `seek_read`/`seek_write` moving one where
/// `pread`/`pwrite` do not.
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
