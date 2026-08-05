//! An in-memory [`Mountable`] backend.
//!
//! The whole tree lives behind `Arc<Mutex<..>>` links so that every operation
//! shares one store through `&self`, and so the backend is `Send + Sync` as
//! [`Mountable`] requires — enough for tests, scratch space, and prototyping.
//!
//! Like [`PassthroughVolume`](super::PassthroughVolume), the data plane is
//! reached through an open [`FileHandle`]: `open` hands out an [`InMemHandle`]
//! sharing the file's byte buffer with the tree, so positioned writes and
//! truncations are visible to later `stat`s. Creation is part of that same `open`
//! (via [`OpenOptions`]), so there is exactly one path by which bytes enter the
//! store — and `create_new` can be genuinely exclusive.

use std::collections::HashMap;
use std::io;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::lock::lock;
use crate::{
    CortexError, Dirent, DirentKind, FileExt, FileHandle, Mountable, OpenOptions, Result, Stat,
};

/// The largest file this volume will represent — a safety ceiling, not a capacity
/// plan.
///
/// The tree lives in the host's address space, and a write's size comes from an
/// offset the *guest* chooses (virtio-fs bounds the byte count, not the offset).
/// Without a ceiling one `dd seek=…` makes the host allocate arbitrarily and
/// abort, and nothing catches the unwind between here and the virtio-fs worker.
/// Raise it if a workload legitimately needs bigger files in RAM.
const MAX_FILE_SIZE: u64 = 1 << 30;

/// What a handle answers when asked to do what its open did not allow. Identical
/// on every POSIX system, and `libc` is only an optional dependency.
const EBADF: i32 = 9;

/// Bound a requested end-of-file against [`MAX_FILE_SIZE`]. Overflow counts as
/// exceeding it: both mean a size this volume will not represent.
fn checked_end(offset: u64, len: usize) -> Result<usize> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= MAX_FILE_SIZE => Ok(end as usize),
        _ => Err(CortexError::FileTooLarge),
    }
}

/// A file's bytes together with the moment they last changed.
///
/// The two live under one lock deliberately. An open [`InMemHandle`] shares only
/// this body — it has no route to the tree node — so a file's mtime has to be
/// reachable from here or a write could not advance it. And keeping them under
/// *one* lock is what stops an observer seeing new bytes beside an old mtime: a
/// guest that negotiated `AUTO_INVAL_DATA` decides from mtime alone when to drop
/// cached pages, so that pairing would leave it caching the new bytes forever.
struct FileBody {
    bytes: Vec<u8>,
    mtime: SystemTime,
    /// Set once at creation. Nothing here changes a birth time.
    created: SystemTime,
}

impl FileBody {
    fn new(bytes: Vec<u8>) -> Self {
        let now = SystemTime::now();
        FileBody {
            bytes,
            mtime: now,
            created: now,
        }
    }

    /// Record that the bytes just changed. Under the caller's existing lock, so
    /// the size and the timestamp become visible together.
    fn touch(&mut self) {
        self.mtime = SystemTime::now();
    }

    fn stat(&self) -> Stat {
        // `atime`/`ctime` stay unset: `attr_for` falls back to `mtime` for both,
        // and an access time would mean a write on every read.
        Stat {
            mtime: Some(self.mtime),
            created: Some(self.created),
            ..Stat::new(DirentKind::File, self.bytes.len() as u64)
        }
    }
}

/// A file's body, shared between its tree node and every open handle.
type FileData = Arc<Mutex<FileBody>>;

/// An interior-mutable link to a tree node, shared through the whole store.
type Link = Arc<Mutex<Node>>;

enum Node {
    /// A directory's mtime lives here rather than in a shared body: nothing but
    /// the tree ever changes a directory, so there is no handle to reach it from.
    Dir {
        children: HashMap<String, Link>,
        mtime: SystemTime,
        created: SystemTime,
    },
    File {
        data: FileData,
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

    fn new_file(bytes: Vec<u8>) -> Link {
        Arc::new(Mutex::new(Node::File {
            data: Arc::new(Mutex::new(FileBody::new(bytes))),
        }))
    }

    /// The metadata this node reports. The caller already holds its lock.
    fn stat(&self) -> Stat {
        match self {
            Node::Dir { mtime, created, .. } => Stat {
                mtime: Some(*mtime),
                created: Some(*created),
                ..Stat::new(DirentKind::Dir, 0)
            },
            Node::File { data } => lock(data).stat(),
        }
    }

    /// Record that this directory's set of names just changed — a child added or
    /// removed, which POSIX counts as modifying the directory itself. Writing a
    /// child's *contents* does not.
    fn touch_dir(&mut self) {
        if let Node::Dir { mtime, .. } = self {
            *mtime = SystemTime::now();
        }
    }
}

/// An in-memory volume: a directory tree held entirely in RAM.
pub struct InMemVolume {
    root: Link,
}

impl InMemVolume {
    /// Create a fresh, empty in-memory volume.
    pub fn new() -> Self {
        InMemVolume {
            root: Node::new_dir(),
        }
    }

    /// Resolve `path`'s parent directory and final name in one step, which is
    /// what every mutating operation needs.
    fn parent_of(&self, path: &Path) -> Result<(Link, String)> {
        let comps = components(path)?;
        let (parent, name) = split_last(&comps)?;
        Ok((self.navigate(parent)?, name.clone()))
    }

    /// Detach the entry at `path` from its parent, provided it is of `expect` kind
    /// (and, for a directory, empty).
    ///
    /// `unlink` and `rmdir` are one operation with two guards: dropping the
    /// parent's last reference is what deletes the node either way.
    fn remove(&self, path: &Path, expect: DirentKind) -> Result<()> {
        let comps = components(path)?;
        let (parent, name) = split_last(&comps)?;
        let dir = self.navigate(parent)?;
        let mut node = lock(&dir);
        // Scoped so the borrow of `children` ends before the removal — and before
        // `touch_dir`, which needs the node back.
        {
            let Node::Dir { children, .. } = &mut *node else {
                return Err(CortexError::NotADirectory);
            };
            let target = children.get(name).ok_or(CortexError::NotFound)?;
            match (&*lock(target), expect) {
                (Node::Dir { children, .. }, DirentKind::Dir) if !children.is_empty() => {
                    return Err(CortexError::NotEmpty);
                }
                (Node::Dir { .. }, DirentKind::File) => return Err(CortexError::IsADirectory),
                (Node::File { .. }, DirentKind::Dir) => return Err(CortexError::NotADirectory),
                _ => {}
            }
            children.remove(name);
        }
        node.touch_dir();
        Ok(())
    }

    /// Walk from the root to the node addressed by `comps`. Every intermediate
    /// component must be a directory.
    fn navigate(&self, comps: &[String]) -> Result<Link> {
        let mut cur = self.root.clone();
        for name in comps {
            let next = match &*lock(&cur) {
                Node::Dir { children, .. } => {
                    children.get(name).cloned().ok_or(CortexError::NotFound)?
                }
                Node::File { .. } => return Err(CortexError::NotADirectory),
            };
            cur = next;
        }
        Ok(cur)
    }
}

impl Default for InMemVolume {
    fn default() -> Self {
        Self::new()
    }
}

impl Mountable for InMemVolume {
    type Handle = InMemHandle;

    fn stat(&self, path: &Path) -> Result<Stat> {
        let comps = components(path)?;
        let link = self.navigate(&comps)?;
        Ok(lock(&link).stat())
    }

    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let comps = components(path)?;
        let link = self.navigate(&comps)?;
        let node = lock(&link);
        match &*node {
            // The child's lock is already taken to learn its kind, and its size
            // and timestamps are right there behind it, so full metadata is free
            // here — a consumer that had to re-`stat` every name would pay an N+1.
            Node::Dir { children, .. } => Ok(children
                .iter()
                .map(|(name, child)| Dirent::with_stat(name, lock(child).stat()))
                .collect()),
            Node::File { .. } => Err(CortexError::NotADirectory),
        }
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        let comps = components(path)?;
        let (parent, name) = split_last(&comps)?;
        let dir = self.navigate(parent)?;
        let mut node = lock(&dir);
        match &mut *node {
            Node::Dir { children, .. } => match children.get(name) {
                Some(existing) => match &*lock(existing) {
                    // Idempotent, matching the platform backends. The workspace
                    // above answers `AlreadyExists` for a *synthesized* directory,
                    // which is a different question.
                    Node::Dir { .. } => Ok(()),
                    Node::File { .. } => Err(CortexError::AlreadyExists),
                },
                None => {
                    children.insert(name.clone(), Node::new_dir());
                    node.touch_dir();
                    Ok(())
                }
            },
            Node::File { .. } => Err(CortexError::NotADirectory),
        }
    }

    fn unlink(&self, path: &Path) -> Result<()> {
        self.remove(path, DirentKind::File)
    }

    fn rmdir(&self, path: &Path) -> Result<()> {
        self.remove(path, DirentKind::Dir)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let (from_comps, to_comps) = (components(from)?, components(to)?);

        // Onto itself is a no-op, checked before anything is detached — POSIX says
        // a rename where both names refer to the same file changes nothing, and
        // going through the move would delete the entry and then re-add it.
        if from_comps == to_comps {
            return Ok(());
        }
        // Into its own descendant would detach the subtree from the tree, leaving a
        // cycle reachable from nothing. `EINVAL`, as `fs::rename` gives.
        if to_comps.starts_with(&from_comps) {
            return Err(CortexError::InvalidArgument);
        }

        let (from_dir, from_name) = self.parent_of(from)?;
        let (to_dir, to_name) = self.parent_of(to)?;

        // Detach under the source parent's lock, then attach under the
        // destination's. Taking both at once would deadlock whenever two renames
        // crossed the same pair of directories in opposite directions.
        let moving = {
            let node = lock(&from_dir);
            let Node::Dir { children, .. } = &*node else {
                return Err(CortexError::NotADirectory);
            };
            children
                .get(&from_name)
                .cloned()
                .ok_or(CortexError::NotFound)?
        };
        let moving_is_dir = matches!(&*lock(&moving), Node::Dir { .. });

        // The destination decides whether this is legal at all, so it is checked
        // before the source is detached: a refusal must leave the tree untouched.
        {
            let mut node = lock(&to_dir);
            let Node::Dir { children, .. } = &mut *node else {
                return Err(CortexError::NotADirectory);
            };
            if let Some(existing) = children.get(&to_name) {
                match (&*lock(existing), moving_is_dir) {
                    // A directory may only replace an *empty* directory.
                    (Node::Dir { children, .. }, true) if !children.is_empty() => {
                        return Err(CortexError::NotEmpty);
                    }
                    (Node::Dir { .. }, false) => return Err(CortexError::IsADirectory),
                    (Node::File { .. }, true) => return Err(CortexError::NotADirectory),
                    // file over file, or directory over empty directory: replaced.
                    _ => {}
                }
            }
            children.insert(to_name, Arc::clone(&moving));
            node.touch_dir();
        }

        // Only now does the old name go. If the two parents are the same node this
        // re-locks it, which is why the destination's guard is already released.
        let mut node = lock(&from_dir);
        if let Node::Dir { children, .. } = &mut *node {
            children.remove(&from_name);
        }
        node.touch_dir();
        Ok(())
    }

    fn open(&self, path: &Path, options: OpenOptions) -> Result<(Self::Handle, Stat)> {
        options.validate()?;

        // Creation and the existence check happen under the parent's lock, so
        // `create_new` is genuinely exclusive: no other thread can slip an entry
        // in between the two.
        let data = if options.create {
            let (dir, name) = self.parent_of(path)?;
            let mut node = lock(&dir);
            let created;
            let data = {
                let Node::Dir { children, .. } = &mut *node else {
                    return Err(CortexError::NotADirectory);
                };
                match children.get(&name) {
                    Some(existing) => {
                        if options.create_new {
                            return Err(CortexError::AlreadyExists);
                        }
                        created = false;
                        match &*lock(existing) {
                            Node::File { data } => data.clone(),
                            Node::Dir { .. } => return Err(CortexError::IsADirectory),
                        }
                    }
                    None => {
                        let file = Node::new_file(Vec::new());
                        let data = match &*lock(&file) {
                            Node::File { data } => data.clone(),
                            Node::Dir { .. } => unreachable!("just built a file node"),
                        };
                        children.insert(name, file);
                        created = true;
                        data
                    }
                }
            };
            // A new *name* modifies the directory; reusing an existing one does not.
            if created {
                node.touch_dir();
            }
            data
        } else {
            let link = self.navigate(&components(path)?)?;
            match &*lock(&link) {
                Node::File { data } => data.clone(),
                Node::Dir { .. } => return Err(CortexError::IsADirectory),
            }
        };

        // Truncate before the handle exists, so the metadata reported back is
        // already that of the empty file. `O_TRUNC` rides the open rather than
        // arriving as its own call, so this is the second path to a resize and
        // has to stamp the file like the first.
        let stat = {
            let mut body = lock(&data);
            if options.truncate && !body.bytes.is_empty() {
                body.bytes.clear();
                body.touch();
            }
            body.stat()
        };
        Ok((
            InMemHandle {
                data,
                read: options.read,
                write: options.write,
                append: options.append,
            },
            stat,
        ))
    }
}

/// An open handle over an in-memory file. It shares the file's byte buffer with
/// the tree (and with any other handle on the same file), so positioned writes
/// and truncations are seen by later `stat`s, `open`s, and reads.
pub struct InMemHandle {
    data: FileData,
    read: bool,
    write: bool,
    append: bool,
}

impl FileExt for InMemHandle {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        if !self.read {
            return Err(io::Error::from_raw_os_error(EBADF));
        }
        // No `touch`: a read is not a modification, and an access time would put a
        // write on the read path for something `attr_for` already derives.
        let body = lock(&self.data);
        let offset = offset as usize;
        if offset >= body.bytes.len() {
            return Ok(0);
        }
        let n = (body.bytes.len() - offset).min(buf.len());
        buf[..n].copy_from_slice(&body.bytes[offset..offset + n]);
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        if !self.write {
            return Err(io::Error::from_raw_os_error(EBADF));
        }
        let mut body = lock(&self.data);
        let offset = if self.append {
            body.bytes.len() as u64
        } else {
            offset
        };
        // Bound before allocating: `offset` is the guest's choice, and `resize`
        // would honour it literally.
        let end = checked_end(offset, buf.len())
            .map_err(|_| io::Error::from(io::ErrorKind::FileTooLarge))?;
        if end > body.bytes.len() {
            body.bytes.resize(end, 0);
        }
        body.bytes[end - buf.len()..end].copy_from_slice(buf);
        // Under the same lock as the bytes, so no observer can pair the new
        // contents with the old timestamp.
        body.touch();
        Ok(buf.len())
    }
}

impl FileHandle for InMemHandle {
    fn truncate(&self, size: u64) -> Result<()> {
        // `InvalidArgument`, which is what `ftruncate` gives a read-only
        // descriptor and so what `PassthroughVolume` answers.
        if !self.write {
            return Err(CortexError::InvalidArgument);
        }
        // Reaches the same `resize`, so it needs the same ceiling.
        let size = checked_end(size, 0)?;
        let mut body = lock(&self.data);
        if body.bytes.len() != size {
            body.bytes.resize(size, 0);
            body.touch();
        }
        Ok(())
    }
}

/// Normalize a path into its plain-name components, rejecting anything that
/// isn't a straightforward absolute-or-relative path (`.`/root are ignored,
/// `..`, prefixes, and non-UTF-8 names are errors).
fn components(path: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for comp in path.components() {
        match comp {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                let name = name.to_str().ok_or(CortexError::InvalidName)?;
                out.push(name.to_string());
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(CortexError::InvalidName);
            }
        }
    }
    Ok(out)
}

/// Split `comps` into its parent components and final name; the empty path
/// (the root) has no name and is rejected.
fn split_last(comps: &[String]) -> Result<(&[String], &String)> {
    match comps.split_last() {
        Some((name, parent)) => Ok((parent, name)),
        None => Err(CortexError::InvalidName),
    }
}

// Tests live beside this file rather than inside it: they had grown longer than
// the implementation, so a reader opening it had to scroll past them to find the
// code. They are still a child module, so private items stay reachable.
#[cfg(test)]
#[path = "inmem_tests.rs"]
mod tests;
