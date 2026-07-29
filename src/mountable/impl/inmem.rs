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

/// Bound a requested end-of-file against [`MAX_FILE_SIZE`]. Overflow counts as
/// exceeding it: both mean a size this volume will not represent.
fn checked_end(offset: u64, len: usize) -> Result<usize> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= MAX_FILE_SIZE => Ok(end as usize),
        _ => Err(CortexError::FileTooLarge),
    }
}

/// A file's byte buffer, shared between its tree node and every open handle.
type FileData = Arc<Mutex<Vec<u8>>>;

/// An interior-mutable link to a tree node, shared through the whole store.
type Link = Arc<Mutex<Node>>;

enum Node {
    Dir { children: HashMap<String, Link> },
    File { data: FileData },
}

impl Node {
    fn new_dir() -> Link {
        Arc::new(Mutex::new(Node::Dir {
            children: HashMap::new(),
        }))
    }

    fn new_file(data: Vec<u8>) -> Link {
        Arc::new(Mutex::new(Node::File {
            data: Arc::new(Mutex::new(data)),
        }))
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
        let Node::Dir { children } = &mut *node else {
            return Err(CortexError::NotADirectory);
        };
        // Scoped so the borrow of `children` ends before the removal.
        {
            let target = children.get(name).ok_or(CortexError::NotFound)?;
            match (&*lock(target), expect) {
                (Node::Dir { children }, DirentKind::Dir) if !children.is_empty() => {
                    return Err(CortexError::NotEmpty);
                }
                (Node::Dir { .. }, DirentKind::File) => return Err(CortexError::IsADirectory),
                (Node::File { .. }, DirentKind::Dir) => return Err(CortexError::NotADirectory),
                _ => {}
            }
        }
        children.remove(name);
        Ok(())
    }

    /// Walk from the root to the node addressed by `comps`. Every intermediate
    /// component must be a directory.
    fn navigate(&self, comps: &[String]) -> Result<Link> {
        let mut cur = self.root.clone();
        for name in comps {
            let next = match &*lock(&cur) {
                Node::Dir { children } => {
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
        let node = lock(&link);
        match &*node {
            Node::Dir { .. } => Ok(Stat::new(DirentKind::Dir, 0)),
            Node::File { data } => {
                let size = lock(data).len() as u64;
                Ok(Stat::new(DirentKind::File, size))
            }
        }
    }

    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let comps = components(path)?;
        let link = self.navigate(&comps)?;
        let node = lock(&link);
        match &*node {
            // The child's lock is already taken to learn its kind, and its size
            // is right there behind it, so full metadata is free here.
            Node::Dir { children } => Ok(children
                .iter()
                .map(|(name, child)| match &*lock(child) {
                    Node::Dir { .. } => Dirent::with_stat(name, Stat::new(DirentKind::Dir, 0)),
                    Node::File { data } => Dirent::with_stat(
                        name,
                        Stat::new(DirentKind::File, lock(data).len() as u64),
                    ),
                })
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
            Node::Dir { children } => match children.get(name) {
                Some(existing) => match &*lock(existing) {
                    Node::Dir { .. } => Ok(()),
                    Node::File { .. } => Err(CortexError::AlreadyExists),
                },
                None => {
                    children.insert(name.clone(), Node::new_dir());
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

    fn open(&self, path: &Path, options: OpenOptions) -> Result<(Self::Handle, Stat)> {
        options.validate()?;

        // Creation and the existence check happen under the parent's lock, so
        // `create_new` is genuinely exclusive: no other thread can slip an entry
        // in between the two.
        let data = if options.create || options.create_new {
            let (dir, name) = self.parent_of(path)?;
            let mut node = lock(&dir);
            let Node::Dir { children } = &mut *node else {
                return Err(CortexError::NotADirectory);
            };
            match children.get(&name) {
                Some(existing) => {
                    if options.create_new {
                        return Err(CortexError::AlreadyExists);
                    }
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
                    data
                }
            }
        } else {
            let link = self.navigate(&components(path)?)?;
            match &*lock(&link) {
                Node::File { data } => data.clone(),
                Node::Dir { .. } => return Err(CortexError::IsADirectory),
            }
        };

        // Truncate before the handle exists, so the metadata reported back is
        // already that of the empty file.
        let size = {
            let mut bytes = lock(&data);
            if options.truncate {
                bytes.clear();
            }
            bytes.len() as u64
        };
        Ok((InMemHandle { data }, Stat::new(DirentKind::File, size)))
    }
}

/// An open handle over an in-memory file. It shares the file's byte buffer with
/// the tree (and with any other handle on the same file), so positioned writes
/// and truncations are seen by later `stat`s, `open`s, and reads.
pub struct InMemHandle {
    data: FileData,
}

impl FileExt for InMemHandle {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let data = lock(&self.data);
        let offset = offset as usize;
        if offset >= data.len() {
            return Ok(0);
        }
        let n = (data.len() - offset).min(buf.len());
        buf[..n].copy_from_slice(&data[offset..offset + n]);
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        // Bound before allocating: `offset` is the guest's choice, and `resize`
        // would honour it literally.
        let end = checked_end(offset, buf.len())
            .map_err(|_| io::Error::from(io::ErrorKind::FileTooLarge))?;
        let mut data = lock(&self.data);
        if end > data.len() {
            data.resize(end, 0);
        }
        data[end - buf.len()..end].copy_from_slice(buf);
        Ok(buf.len())
    }
}

impl FileHandle for InMemHandle {
    fn truncate(&self, size: u64) -> Result<()> {
        // Reaches the same `resize`, so it needs the same ceiling.
        let size = checked_end(size, 0)?;
        lock(&self.data).resize(size, 0);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(vol: &InMemVolume, path: &str) -> Vec<String> {
        let mut names: Vec<_> = vol
            .list(Path::new(path))
            .unwrap()
            .iter()
            .map(|e| e.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Create a file and fill it with `data` in one step.
    fn write_file(vol: &InMemVolume, path: &str, data: &[u8]) {
        let (handle, _) = vol
            .open(
                Path::new(path),
                OpenOptions {
                    create_new: true,
                    ..OpenOptions::read_write()
                },
            )
            .unwrap();
        handle.write_all_at(data, 0).unwrap();
    }

    fn read_file(vol: &InMemVolume, path: &str) -> Vec<u8> {
        let (handle, stat) = vol
            .open(Path::new(path), OpenOptions::read_only())
            .unwrap();
        let mut buf = vec![0u8; stat.size as usize];
        handle.read_exact_at(&mut buf, 0).unwrap();
        buf
    }

    #[test]
    fn create_read_list_unlink() {
        let vol = InMemVolume::new();
        vol.mkdir(Path::new("/sub")).unwrap();
        write_file(&vol, "/hello.txt", b"world");
        write_file(&vol, "/sub/inner", b"hi");

        assert_eq!(read_file(&vol, "/hello.txt"), b"world");
        assert_eq!(read_file(&vol, "/sub/inner"), b"hi");
        assert_eq!(names(&vol, "/"), vec!["hello.txt", "sub"]);
        assert_eq!(names(&vol, "/sub"), vec!["inner"]);

        vol.unlink(Path::new("/hello.txt")).unwrap();
        assert!(matches!(
            vol.open(Path::new("/hello.txt"), OpenOptions::read_write()),
            Err(CortexError::NotFound)
        ));
    }

    #[test]
    fn positioned_writes_and_truncate_are_shared() {
        let vol = InMemVolume::new();
        write_file(&vol, "/f", b"world");

        // A second handle sees writes made through the first, and both share the
        // tree's buffer, so `stat` reflects the new size.
        let (a, _) = vol.open(Path::new("/f"), OpenOptions::read_write()).unwrap();
        let (b, _) = vol.open(Path::new("/f"), OpenOptions::read_write()).unwrap();
        a.write_all_at(b"HELLO", 0).unwrap();
        let mut buf = [0u8; 5];
        b.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"HELLO");

        // Growth zero-fills; a positioned write past EOF extends the file.
        a.write_all_at(b"!", 6).unwrap();
        assert_eq!(read_file(&vol, "/f"), b"HELLO\0!");

        a.truncate(3).unwrap();
        assert_eq!(vol.stat(Path::new("/f")).unwrap().size, 3);
        assert_eq!(read_file(&vol, "/f"), b"HEL");
    }

    #[test]
    fn open_options_pin_the_creation_contract() {
        let vol = InMemVolume::new();
        let rw = OpenOptions::read_write();
        let create = OpenOptions { create: true, ..rw };
        let create_new = OpenOptions {
            create_new: true,
            ..create
        };

        // `open` reports the entry's metadata in the same call that hands out
        // the handle: a FUSE `create` must answer with attributes *and* an `fh`
        // in one message, and a follow-up `stat` would be both a second backend
        // round trip and a window for the entry to be replaced underneath.
        let (handle, stat) = vol.open(Path::new("/f"), create).unwrap();
        assert_eq!(stat.kind, DirentKind::File);
        assert_eq!(stat.size, 0);
        handle.write_all_at(b"hello", 0).unwrap();

        // `create` on something that already exists simply opens it.
        let (_, stat) = vol.open(Path::new("/f"), create).unwrap();
        assert_eq!(stat.size, 5);

        // `create_new` is the exclusive one. This check has to belong to the
        // backend: decomposing it into `stat`-then-create would be a race, and
        // for a remote backend the atomic form is the only one that exists
        // (a conditional PUT, `O_EXCL`) — which is why the options travel with
        // the call instead of `create` being a separate operation.
        assert!(matches!(
            vol.open(Path::new("/f"), create_new),
            Err(CortexError::AlreadyExists)
        ));

        // Truncation happens at open time, before the handle exists, so the
        // metadata reported back already reflects it.
        let (_, stat) = vol
            .open(Path::new("/f"), OpenOptions { truncate: true, ..rw })
            .unwrap();
        assert_eq!(stat.size, 0);

        // A missing parent is never created implicitly.
        assert!(matches!(
            vol.open(Path::new("/missing/f"), create_new),
            Err(CortexError::NotFound)
        ));

        // Without `create`, `open` opens only what is already there.
        assert!(matches!(
            vol.open(Path::new("/nope"), rw),
            Err(CortexError::NotFound)
        ));

        // A directory never becomes a file handle, whatever the options ask.
        vol.mkdir(Path::new("/d")).unwrap();
        assert!(matches!(
            vol.open(Path::new("/d"), create),
            Err(CortexError::IsADirectory)
        ));

        // `O_RDONLY | O_CREAT` is legal POSIX and stays legal here.
        let (_, stat) = vol
            .open(
                Path::new("/ro"),
                OpenOptions {
                    create: true,
                    ..OpenOptions::read_only()
                },
            )
            .unwrap();
        assert_eq!(stat.size, 0);

        // Asking for neither read nor write leaves nothing the handle can do.
        assert!(matches!(
            vol.open(Path::new("/f"), OpenOptions::default()),
            Err(CortexError::InvalidArgument)
        ));
    }

    #[test]
    fn absurd_offsets_are_refused_rather_than_allocated() {
        let vol = InMemVolume::new();
        let (handle, _) = vol.open(Path::new("/f"), OpenOptions { create_new: true, ..OpenOptions::read_write() }).unwrap();
        handle.write_all_at(b"keep", 0).unwrap();

        // virtio-fs caps the byte *count* at 1 MiB but passes the guest's
        // `offset` through untouched, so the offset is attacker-controlled.
        // Growing the buffer to meet it would abort the host process, and there
        // is no `catch_unwind` between here and the virtio-fs worker thread.
        let err = handle.write_at(b"x", 1 << 45).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);

        // Near the top of the range the `offset + len` addition itself wraps.
        let err = handle.write_at(b"x", u64::MAX).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);

        // `truncate` reaches the same `resize`, so it needs the same guard.
        assert!(matches!(
            handle.truncate(1 << 45),
            Err(CortexError::FileTooLarge)
        ));

        // A rejected call leaves the file exactly as it was.
        assert_eq!(vol.stat(Path::new("/f")).unwrap().size, 4);
        assert_eq!(read_file(&vol, "/f"), b"keep");

        // A write that lands inside the limit still works.
        handle.write_all_at(b"!", 4).unwrap();
        assert_eq!(read_file(&vol, "/f"), b"keep!");
    }

    #[test]
    fn unlink_takes_files_and_rmdir_takes_empty_directories() {
        let vol = InMemVolume::new();
        vol.mkdir(Path::new("/dir")).unwrap();
        write_file(&vol, "/dir/child", b"x");
        write_file(&vol, "/file", b"y");

        // Each call refuses the other's kind, exactly as the two syscalls do.
        assert!(matches!(
            vol.unlink(Path::new("/dir")),
            Err(CortexError::IsADirectory)
        ));
        assert!(matches!(
            vol.rmdir(Path::new("/file")),
            Err(CortexError::NotADirectory)
        ));
        // A directory with children is never discarded implicitly.
        assert!(matches!(
            vol.rmdir(Path::new("/dir")),
            Err(CortexError::NotEmpty)
        ));
        assert_eq!(names(&vol, "/dir"), vec!["child"]);

        // The `rm -rf` sequence a kernel actually sends: empty it, then drop it.
        vol.unlink(Path::new("/dir/child")).unwrap();
        vol.rmdir(Path::new("/dir")).unwrap();
        assert!(matches!(
            vol.stat(Path::new("/dir")),
            Err(CortexError::NotFound)
        ));

        assert!(matches!(
            vol.rmdir(Path::new("/dir")),
            Err(CortexError::NotFound)
        ));
    }

    #[test]
    fn kind_errors() {
        let vol = InMemVolume::new();
        vol.mkdir(Path::new("/dir")).unwrap();
        write_file(&vol, "/file", b"x");

        assert!(matches!(
            vol.open(Path::new("/dir"), OpenOptions::read_write()),
            Err(CortexError::IsADirectory)
        ));
        assert!(matches!(
            vol.list(Path::new("/file")),
            Err(CortexError::NotADirectory)
        ));
        assert!(matches!(
            vol.mkdir(Path::new("/file")),
            Err(CortexError::AlreadyExists)
        ));
        assert!(matches!(
            vol.open(Path::new("/file"), OpenOptions { create_new: true, ..OpenOptions::read_write() }),
            Err(CortexError::AlreadyExists)
        ));
    }

    #[test]
    fn missing_and_invalid_paths_rejected() {
        let vol = InMemVolume::new();
        assert!(matches!(
            vol.stat(Path::new("/nope")),
            Err(CortexError::NotFound)
        ));
        assert!(matches!(
            vol.open(Path::new("/missing/deep"), OpenOptions { create_new: true, ..OpenOptions::read_write() }),
            Err(CortexError::NotFound)
        ));
        assert!(matches!(
            vol.mkdir(Path::new("/a/../b")),
            Err(CortexError::InvalidName)
        ));
    }
}
