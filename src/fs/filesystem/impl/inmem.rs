//! An in-memory [`FileSystem`] store.

use std::{
    collections::HashMap,
    io,
    path::{Component, Path},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use crate::{
    BoxFuture,
    fs::{Dirent, DirentKind, FileSystem, Stat},
    lock::lock,
};

/// The largest file this store will represent — a safety ceiling, not a capacity plan.
///
/// The tree lives in the host's address space, and a write's end comes from an offset the
/// *guest* chooses (virtio-fs bounds the byte count, not the offset). Without a ceiling one
/// `dd seek=…` makes the host allocate arbitrarily and abort, with nothing catching the unwind
/// before the virtio-fs worker.
const MAX_FILE_SIZE: u64 = 1 << 30;

/// Bound a requested end-of-file against [`MAX_FILE_SIZE`]; overflow counts as exceeding it.
fn checked_end(offset: u64, len: usize) -> io::Result<usize> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= MAX_FILE_SIZE => Ok(end as usize),
        _ => Err(io::ErrorKind::FileTooLarge.into()),
    }
}

/// An interior-mutable link to a tree node, shared through the whole store, so every operation
/// works through `&self` and the store is `Send + Sync`.
type Link = Arc<Mutex<Node>>;

enum Node {
    Dir {
        children: HashMap<String, Link>,
        mtime: SystemTime,
        created: SystemTime,
    },
    /// The bytes and the moment they last changed, under the node's one lock.
    ///
    /// Together so no observer sees new bytes beside an old mtime: a guest that negotiated
    /// `AUTO_INVAL_DATA` drops cached pages on mtime alone, and would otherwise keep stale
    /// pages forever.
    File {
        bytes: Vec<u8>,
        mtime: SystemTime,
        /// Birth time; never changes.
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

    /// The caller already holds this node's lock.
    fn stat(&self) -> Stat {
        // `atime`/`ctime` stay unset (consumers fall back to `mtime`): tracking access time
        // would put a write on every read.
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

    /// Record that this node changed: a directory's set of names, or a file's bytes.
    ///
    /// POSIX counts adding or removing a child as modifying the directory; writing a child's
    /// contents does not, so a write touches the file and not its parent.
    fn touch(&mut self) {
        match self {
            Node::Dir { mtime, .. } | Node::File { mtime, .. } => *mtime = SystemTime::now(),
        }
    }
}

/// A directory tree held entirely in RAM, for tests, scratch space, and prototyping.
pub struct InMemFs {
    root: Link,
}

impl InMemFs {
    /// An empty store.
    pub fn new() -> Self {
        InMemFs {
            root: Node::new_dir(),
        }
    }

    /// Whether anything is at `path`.
    pub(crate) fn contains(&self, path: &Path) -> io::Result<bool> {
        match self.navigate(&components(path)?) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// `mkdir -p`: every missing directory along `path` is made, and one already there is
    /// left alone.
    pub(crate) fn mkdir_all(&self, path: &Path) -> io::Result<()> {
        self.dir_all(&components(path)?).map(drop)
    }

    /// Put a file at `path` holding everything `content` yields, making the directories on
    /// the way and replacing a file already there.
    ///
    /// `content` is read in full before the tree is touched, so a reader failing partway
    /// leaves whatever was there.
    pub(crate) fn put_file(&self, path: &Path, content: impl io::Read) -> io::Result<()> {
        let comps = components(path)?;
        let (parent, name) = split_last(&comps)?;

        // One byte past the ceiling is enough to know the ceiling was passed.
        let mut bytes = Vec::new();
        io::Read::read_to_end(&mut content.take(MAX_FILE_SIZE + 1), &mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_SIZE {
            return Err(io::ErrorKind::FileTooLarge.into());
        }

        let dir = self.dir_all(parent)?;
        let mut node = lock(&dir);
        let Node::Dir { children, .. } = &mut *node else {
            unreachable!("`dir_all` only returns directories");
        };
        if let Some(existing) = children.get(name)
            && matches!(&*lock(existing), Node::Dir { .. })
        {
            return Err(io::ErrorKind::IsADirectory.into());
        }
        let now = SystemTime::now();
        children.insert(
            name.clone(),
            Arc::new(Mutex::new(Node::File {
                bytes,
                mtime: now,
                created: now,
            })),
        );
        node.touch();
        Ok(())
    }

    /// Remove the file at `path`; a directory there is refused.
    pub(crate) fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.remove(path, DirentKind::File)
    }

    /// Walk `comps` from the root, making missing directories, and return the last. A file
    /// anywhere along it is `NotADirectory`.
    fn dir_all(&self, comps: &[String]) -> io::Result<Link> {
        let mut cur = self.root.clone();
        for name in comps {
            let next = {
                let mut node = lock(&cur);
                let Node::Dir { children, .. } = &mut *node else {
                    return Err(io::ErrorKind::NotADirectory.into());
                };
                match children.get(name) {
                    Some(child) => child.clone(),
                    None => {
                        let child = Node::new_dir();
                        children.insert(name.clone(), child.clone());
                        node.touch();
                        child
                    }
                }
            };
            cur = next;
        }
        if matches!(&*lock(&cur), Node::File { .. }) {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        Ok(cur)
    }

    /// Walk from the root to the node addressed by `comps`.
    ///
    /// Every call walks from the root, as a path-addressed store has no open to amortize it
    /// across; in RAM that is one hash lookup per component.
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

    /// The file at `path`, for the data plane. A directory is refused here because a kernel
    /// rejects only a *write*-mode open of one itself; a read open comes through for this answer.
    fn file_at(&self, path: &Path) -> io::Result<Link> {
        let link = self.navigate(&components(path)?)?;
        let is_dir = matches!(&*lock(&link), Node::Dir { .. });
        if is_dir {
            return Err(io::ErrorKind::IsADirectory.into());
        }
        Ok(link)
    }

    /// Resolve `path`'s parent directory and final name.
    fn parent_of(&self, path: &Path) -> io::Result<(Link, String)> {
        let comps = components(path)?;
        let (parent, name) = split_last(&comps)?;
        Ok((self.navigate(parent)?, name.clone()))
    }

    /// Insert a fresh node at `path`, refusing a name that is taken.
    ///
    /// Check and insert happen under the parent's lock, so no other thread can slip an entry
    /// in between.
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
    /// Serves both `unlink` and `rmdir`: dropping the parent's reference deletes the node
    /// either way.
    fn remove(&self, path: &Path, expect: DirentKind) -> io::Result<()> {
        let comps = components(path)?;
        let (parent, name) = split_last(&comps)?;
        let dir = self.navigate(parent)?;
        let mut node = lock(&dir);
        // Scoped so the borrow of `children` ends before `touch`.
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

impl FileSystem for InMemFs {
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
                // Full metadata is free: each child's lock is taken for its kind anyway, and
                // it spares consumers an N+1 of `stat`s.
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
            // No `touch`: a read is not a modification.
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
            // Bound before allocating: `offset` is the guest's choice and `resize` honours it.
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
            // Under the bytes' lock, so no observer pairs new contents with the old mtime.
            node.touch();
            Ok(buf.len())
        })
    }

    fn truncate<'a>(&'a self, path: &'a Path, size: u64) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            // Bound before `resize`: `size` is the guest's choice.
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

            // POSIX: renaming onto itself changes nothing. Checked before anything is
            // detached, since the move below would delete and re-add the entry.
            if from_comps == to_comps {
                return Ok(());
            }
            // Into its own descendant would leave an unreachable cycle. `EINVAL`, as
            // `fs::rename` gives.
            if to_comps.starts_with(&from_comps) {
                return Err(io::ErrorKind::InvalidInput.into());
            }

            let (from_dir, from_name) = self.parent_of(from)?;
            let (to_dir, to_name) = self.parent_of(to)?;

            // One parent lock at a time: holding both would deadlock two renames crossing
            // the same pair of directories in opposite directions.
            let moving = {
                let node = lock(&from_dir);
                let Node::Dir { children, .. } = &*node else {
                    return Err(io::ErrorKind::NotADirectory.into());
                };
                children.get(&from_name).cloned().ok_or_else(not_found)?
            };
            let moving_is_dir = matches!(&*lock(&moving), Node::Dir { .. });

            // Attach before detaching, so a refusal by the destination leaves the tree
            // untouched.
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

            // The destination's guard is already released, since both parents may be the
            // same node.
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

/// A path's plain-name components: `.` and root are ignored; `..`, prefixes, and non-UTF-8
/// names are errors.
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

/// Split `comps` into parent components and final name; the root has no name and is rejected.
fn split_last(comps: &[String]) -> io::Result<(&[String], &String)> {
    match comps.split_last() {
        Some((name, parent)) => Ok((parent, name)),
        None => Err(io::ErrorKind::InvalidFilename.into()),
    }
}
