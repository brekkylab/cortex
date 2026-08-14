//! An in-memory [`Mountable`] store.
//!
//! The whole tree lives behind `Arc<Mutex<..>>` links so that every operation shares one
//! store through `&self`, and so the store is `Send + Sync` as [`Mountable`] requires —
//! enough for tests, scratch space, and prototyping.
//!
//! Every path is walked from the root on every call, which is what a store addressed by path
//! is: there is no open to amortize the walk across, and nothing to hold between calls. For a
//! tree in RAM the walk is a hash lookup per component.

use std::{
    collections::HashMap,
    io,
    path::{Component, Path},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use crate::BoxFuture;
use crate::fs::{Dirent, DirentKind, Mountable, Stat};
use crate::lock::lock;

/// The largest file this store will represent — a safety ceiling, not a capacity plan.
///
/// The tree lives in the host's address space, and a write's size comes from an offset the
/// *guest* chooses (virtio-fs bounds the byte count, not the offset). Without a ceiling one
/// `dd seek=…` makes the host allocate arbitrarily and abort, and nothing catches the unwind
/// between here and the virtio-fs worker. Raise it if a workload legitimately needs bigger
/// files in RAM.
const MAX_FILE_SIZE: u64 = 1 << 30;

/// Bound a requested end-of-file against [`MAX_FILE_SIZE`]. Overflow counts as exceeding it:
/// both mean a size this store will not represent.
fn checked_end(offset: u64, len: usize) -> io::Result<usize> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= MAX_FILE_SIZE => Ok(end as usize),
        _ => Err(io::ErrorKind::FileTooLarge.into()),
    }
}

/// An interior-mutable link to a tree node, shared through the whole store.
type Link = Arc<Mutex<Node>>;

enum Node {
    Dir {
        children: HashMap<String, Link>,
        mtime: SystemTime,
        created: SystemTime,
    },
    /// The bytes and the moment they last changed, under the node's one lock.
    ///
    /// Keeping them together is what stops an observer seeing new bytes beside an old mtime:
    /// a guest that negotiated `AUTO_INVAL_DATA` decides from mtime alone when to drop cached
    /// pages, so that pairing would leave it caching the new bytes forever.
    File {
        bytes: Vec<u8>,
        mtime: SystemTime,
        /// Set once at creation. Nothing here changes a birth time.
        created: SystemTime,
    },
}

impl Node {
    fn new_dir() -> Link {
        let now = SystemTime::now();
        Arc::new(Mutex::new(Node::Dir {
            children: HashMap::new(),
            mtime: now,
            created: now,
        }))
    }

    fn new_file() -> Link {
        let now = SystemTime::now();
        Arc::new(Mutex::new(Node::File {
            bytes: Vec::new(),
            mtime: now,
            created: now,
        }))
    }

    /// The metadata this node reports. The caller already holds its lock.
    fn stat(&self) -> Stat {
        // `atime`/`ctime` stay unset: `attr_for` falls back to `mtime` for both, and an
        // access time would mean a write on every read.
        match self {
            Node::Dir { mtime, created, .. } => Stat {
                mtime: Some(*mtime),
                created: Some(*created),
                ..Stat::new(DirentKind::Dir, 0)
            },
            Node::File {
                bytes,
                mtime,
                created,
            } => Stat {
                mtime: Some(*mtime),
                created: Some(*created),
                ..Stat::new(DirentKind::File, bytes.len() as u64)
            },
        }
    }

    /// Record that this node just changed: a directory's set of names, or a file's bytes.
    ///
    /// POSIX counts adding or removing a child as modifying the directory itself. Writing a
    /// child's *contents* does not, which is why a write touches the file and not its parent.
    fn touch(&mut self) {
        match self {
            Node::Dir { mtime, .. } | Node::File { mtime, .. } => *mtime = SystemTime::now(),
        }
    }
}

/// A directory tree held entirely in RAM.
///
/// For tests, scratch space, and prototyping — nothing here reaches a disk or a network.
pub struct InMemFs {
    root: Link,
}

impl InMemFs {
    /// Create a fresh, empty in-memory store.
    pub fn new() -> Self {
        InMemFs {
            root: Node::new_dir(),
        }
    }

    /// Walk from the root to the node addressed by `comps`. Every intermediate component must
    /// be a directory.
    fn navigate(&self, comps: &[String]) -> io::Result<Link> {
        let mut cur = self.root.clone();
        for name in comps {
            let next = match &*lock(&cur) {
                Node::Dir { children, .. } => children.get(name).cloned().ok_or_else(not_found)?,
                Node::File { .. } => return Err(io::ErrorKind::NotADirectory.into()),
            };
            cur = next;
        }
        Ok(cur)
    }

    /// The file at `path`, for the data plane. A directory is refused, which is where a
    /// caller reading one finds out — a kernel rejects a *write*-mode open of a directory
    /// itself, and lets a read open through for exactly this answer.
    fn file_at(&self, path: &Path) -> io::Result<Link> {
        let link = self.navigate(&components(path)?)?;
        let is_dir = matches!(&*lock(&link), Node::Dir { .. });
        if is_dir {
            return Err(io::ErrorKind::IsADirectory.into());
        }
        Ok(link)
    }

    /// Resolve `path`'s parent directory and final name in one step, which is what every
    /// mutating operation needs.
    fn parent_of(&self, path: &Path) -> io::Result<(Link, String)> {
        let comps = components(path)?;
        let (parent, name) = split_last(&comps)?;
        Ok((self.navigate(parent)?, name.clone()))
    }

    /// Insert a fresh node at `path`, refusing a name that is taken.
    ///
    /// The check and the insertion happen under the parent's one lock, which is what makes
    /// this exclusive: no other thread can slip an entry in between the two.
    fn insert(&self, path: &Path, node: Link) -> io::Result<Stat> {
        let (dir, name) = self.parent_of(path)?;
        let mut parent = lock(&dir);
        let Node::Dir { children, .. } = &mut *parent else {
            return Err(io::ErrorKind::NotADirectory.into());
        };
        if children.contains_key(&name) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let stat = lock(&node).stat();
        children.insert(name, node);
        // A new name modifies the directory.
        parent.touch();
        Ok(stat)
    }

    /// Detach the entry at `path` from its parent, provided it is of `expect` kind (and, for a
    /// directory, empty).
    ///
    /// `unlink` and `rmdir` are one operation with two guards: dropping the parent's last
    /// reference is what deletes the node either way.
    fn remove(&self, path: &Path, expect: DirentKind) -> io::Result<()> {
        let comps = components(path)?;
        let (parent, name) = split_last(&comps)?;
        let dir = self.navigate(parent)?;
        let mut node = lock(&dir);
        // Scoped so the borrow of `children` ends before the removal — and before `touch`,
        // which needs the node back.
        {
            let Node::Dir { children, .. } = &mut *node else {
                return Err(io::ErrorKind::NotADirectory.into());
            };
            let target = children.get(name).ok_or_else(not_found)?;
            match (&*lock(target), expect) {
                (Node::Dir { children, .. }, DirentKind::Dir) if !children.is_empty() => {
                    return Err(io::ErrorKind::DirectoryNotEmpty.into());
                }
                (Node::Dir { .. }, DirentKind::File) => {
                    return Err(io::ErrorKind::IsADirectory.into());
                }
                (Node::File { .. }, DirentKind::Dir) => {
                    return Err(io::ErrorKind::NotADirectory.into());
                }
                _ => {}
            }
            children.remove(name);
        }
        node.touch();
        Ok(())
    }
}

impl Default for InMemFs {
    fn default() -> Self {
        Self::new()
    }
}

impl Mountable for InMemFs {
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            let link = self.navigate(&components(path)?)?;
            Ok(lock(&link).stat())
        })
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        Box::pin(async move {
            let link = self.navigate(&components(path)?)?;
            let node = lock(&link);
            match &*node {
                // The child's lock is already taken to learn its kind, and its size and
                // timestamps are right there behind it, so full metadata is free here — a
                // consumer that had to re-`stat` every name would pay an N+1.
                Node::Dir { children, .. } => Ok(children
                    .iter()
                    .map(|(name, child)| Dirent::with_stat(name, lock(child).stat()))
                    .collect()),
                Node::File { .. } => Err(io::ErrorKind::NotADirectory.into()),
            }
        })
    }

    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            let link = self.file_at(path)?;
            let node = lock(&link);
            let Node::File { bytes, .. } = &*node else {
                unreachable!("`file_at` refused a directory");
            };
            // No `touch`: a read is not a modification, and an access time would put a write
            // on the read path for something `attr_for` already derives.
            let offset = offset as usize;
            if offset >= bytes.len() {
                return Ok(0);
            }
            let n = (bytes.len() - offset).min(buf.len());
            buf[..n].copy_from_slice(&bytes[offset..offset + n]);
            Ok(n)
        })
    }

    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move { self.insert(path, Node::new_file()) })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move { self.insert(path, Node::new_dir()) })
    }

    fn unlink<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move { self.remove(path, DirentKind::File) })
    }

    fn rmdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move { self.remove(path, DirentKind::Dir) })
    }

    fn write_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            // Bound before allocating: `offset` is the guest's choice, and `resize` would
            // honour it literally.
            let end = checked_end(offset, buf.len())?;
            let link = self.file_at(path)?;
            let mut node = lock(&link);
            let Node::File { bytes, .. } = &mut *node else {
                unreachable!("`file_at` refused a directory");
            };
            if end > bytes.len() {
                bytes.resize(end, 0);
            }
            bytes[end - buf.len()..end].copy_from_slice(buf);
            // Under the same lock as the bytes, so no observer can pair the new contents with
            // the old timestamp.
            node.touch();
            Ok(buf.len())
        })
    }

    fn truncate<'a>(&'a self, path: &'a Path, size: u64) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            // Reaches the same `resize`, so it needs the same ceiling.
            let size = checked_end(size, 0)?;
            let link = self.file_at(path)?;
            let mut node = lock(&link);
            let Node::File { bytes, .. } = &mut *node else {
                unreachable!("`file_at` refused a directory");
            };
            if bytes.len() == size {
                return Ok(());
            }
            bytes.resize(size, 0);
            node.touch();
            Ok(())
        })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let (from_comps, to_comps) = (components(from)?, components(to)?);

            // Onto itself is a no-op, checked before anything is detached — POSIX says a
            // rename where both names refer to the same file changes nothing, and going
            // through the move would delete the entry and then re-add it.
            if from_comps == to_comps {
                return Ok(());
            }
            // Into its own descendant would detach the subtree from the tree, leaving a cycle
            // reachable from nothing. `EINVAL`, as `fs::rename` gives.
            if to_comps.starts_with(&from_comps) {
                return Err(io::ErrorKind::InvalidInput.into());
            }

            let (from_dir, from_name) = self.parent_of(from)?;
            let (to_dir, to_name) = self.parent_of(to)?;

            // Detach under the source parent's lock, then attach under the destination's.
            // Taking both at once would deadlock whenever two renames crossed the same pair
            // of directories in opposite directions.
            let moving = {
                let node = lock(&from_dir);
                let Node::Dir { children, .. } = &*node else {
                    return Err(io::ErrorKind::NotADirectory.into());
                };
                children.get(&from_name).cloned().ok_or_else(not_found)?
            };
            let moving_is_dir = matches!(&*lock(&moving), Node::Dir { .. });

            // The destination decides whether this is legal at all, so it is checked before
            // the source is detached: a refusal must leave the tree untouched.
            {
                let mut node = lock(&to_dir);
                let Node::Dir { children, .. } = &mut *node else {
                    return Err(io::ErrorKind::NotADirectory.into());
                };
                if let Some(existing) = children.get(&to_name) {
                    match (&*lock(existing), moving_is_dir) {
                        // A directory may only replace an *empty* directory.
                        (Node::Dir { children, .. }, true) if !children.is_empty() => {
                            return Err(io::ErrorKind::DirectoryNotEmpty.into());
                        }
                        (Node::Dir { .. }, false) => return Err(io::ErrorKind::IsADirectory.into()),
                        (Node::File { .. }, true) => {
                            return Err(io::ErrorKind::NotADirectory.into());
                        }
                        // file over file, or directory over empty directory: replaced.
                        _ => {}
                    }
                }
                children.insert(to_name, Arc::clone(&moving));
                node.touch();
            }

            // Only now does the old name go. If the two parents are the same node this
            // re-locks it, which is why the destination's guard is already released.
            let mut node = lock(&from_dir);
            if let Node::Dir { children, .. } = &mut *node {
                children.remove(&from_name);
            }
            node.touch();
            Ok(())
        })
    }
}

fn not_found() -> io::Error {
    io::ErrorKind::NotFound.into()
}

/// Normalize a path into its plain-name components, rejecting anything that isn't a
/// straightforward absolute-or-relative path (`.`/root are ignored, `..`, prefixes, and
/// non-UTF-8 names are errors).
fn components(path: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    for comp in path.components() {
        match comp {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                let name = name.to_str().ok_or(io::ErrorKind::InvalidFilename)?;
                out.push(name.to_string());
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(io::ErrorKind::InvalidFilename.into());
            }
        }
    }
    Ok(out)
}

/// Split `comps` into its parent components and final name; the empty path (the root) has no
/// name and is rejected.
fn split_last(comps: &[String]) -> io::Result<(&[String], &String)> {
    match comps.split_last() {
        Some((name, parent)) => Ok((parent, name)),
        None => Err(io::ErrorKind::InvalidFilename.into()),
    }
}
