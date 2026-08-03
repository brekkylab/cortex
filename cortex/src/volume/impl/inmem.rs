//! An in-memory [`Mountable`] backend.
//!
//! The whole tree lives behind `Arc<Mutex<..>>` links so that every operation
//! shares one store through `&self`, and so the backend is `Send + Sync` as
//! [`Mountable`] requires — enough for tests, scratch space, and prototyping.
//!
//! Like [`PassthroughVolume`](super::PassthroughVolume), the data plane is
//! reached through an open [`FileHandle`]: [`open`](InMemVolume::open) hands out
//! an [`InMemHandle`] that shares the file's byte buffer with the tree, so
//! positioned reads/writes and truncations are visible to later `stat`s and
//! `open`s. Files are created out of band via [`create`](InMemVolume::create)
//! (the in-memory analogue of a FUSE `create`), since the trait's `open` only
//! opens what already exists.

use std::collections::HashMap;
use std::io;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex};

use crate::{
    CortexError, Result,
    volume::{Dirent, DirentKind, FileExt, FileHandle, Mountable, Stat},
};

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

    /// Walk from the root to the node addressed by `comps`. Every intermediate
    /// component must be a directory.
    fn navigate(&self, comps: &[String]) -> Result<Link> {
        let mut cur = self.root.clone();
        for name in comps {
            let next = match &*cur.lock().unwrap() {
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
        let node = link.lock().unwrap();
        match &*node {
            Node::Dir { .. } => Ok(Stat::new(DirentKind::Dir, 0)),
            Node::File { data } => {
                let size = data.lock().unwrap().len() as u64;
                Ok(Stat::new(DirentKind::File, size))
            }
        }
    }

    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let comps = components(path)?;
        let link = self.navigate(&comps)?;
        let node = link.lock().unwrap();
        match &*node {
            Node::Dir { children } => Ok(children
                .iter()
                .map(|(name, child)| match &*child.lock().unwrap() {
                    Node::Dir { .. } => Dirent::Dir(name.clone()),
                    Node::File { .. } => Dirent::File(name.clone()),
                })
                .collect()),
            Node::File { .. } => Err(CortexError::NotADirectory),
        }
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        let comps = components(path)?;
        let (parent, name) = split_last(&comps)?;
        let dir = self.navigate(parent)?;
        let mut node = dir.lock().unwrap();
        match &mut *node {
            Node::Dir { children } => match children.get(name) {
                Some(existing) => match &*existing.lock().unwrap() {
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
        let comps = components(path)?;
        let (parent, name) = split_last(&comps)?;
        let dir = self.navigate(parent)?;
        let mut node = dir.lock().unwrap();
        match &mut *node {
            Node::Dir { children } => children
                .remove(name)
                .map(|_| ())
                .ok_or(CortexError::NotFound),
            Node::File { .. } => Err(CortexError::NotADirectory),
        }
    }

    fn open(&self, path: &Path) -> Result<Self::Handle> {
        let comps = components(path)?;
        let link = self.navigate(&comps)?;
        let node = link.lock().unwrap();
        match &*node {
            Node::File { data } => Ok(InMemHandle { data: data.clone() }),
            Node::Dir { .. } => Err(CortexError::IsADirectory),
        }
    }

    fn create(&self, path: &Path) -> Result<Self::Handle> {
        let comps = components(path)?;
        let (parent, name) = split_last(&comps)?;
        let dir = self.navigate(parent)?;
        let mut node = dir.lock().unwrap();
        match &mut *node {
            Node::Dir { children } => {
                if children.contains_key(name) {
                    return Err(CortexError::AlreadyExists);
                }
                let file = Node::new_file(Vec::new());
                let data = match &*file.lock().unwrap() {
                    Node::File { data } => data.clone(),
                    Node::Dir { .. } => unreachable!("just built a file node"),
                };
                children.insert(name.clone(), file);
                Ok(InMemHandle { data })
            }
            Node::File { .. } => Err(CortexError::NotADirectory),
        }
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
        let data = self.data.lock().unwrap();
        let offset = offset as usize;
        if offset >= data.len() {
            return Ok(0);
        }
        let n = (data.len() - offset).min(buf.len());
        buf[..n].copy_from_slice(&data[offset..offset + n]);
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        let mut data = self.data.lock().unwrap();
        let offset = offset as usize;
        let end = offset + buf.len();
        if end > data.len() {
            data.resize(end, 0);
        }
        data[offset..end].copy_from_slice(buf);
        Ok(buf.len())
    }
}

impl FileHandle for InMemHandle {
    fn truncate(&self, size: u64) -> Result<()> {
        self.data.lock().unwrap().resize(size as usize, 0);
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
            .map(|e| e.name().to_string())
            .collect();
        names.sort();
        names
    }

    /// Create a file and fill it with `data` in one step.
    fn write_file(vol: &InMemVolume, path: &str, data: &[u8]) {
        let handle = vol.create(Path::new(path)).unwrap();
        handle.write_all_at(data, 0).unwrap();
    }

    fn read_file(vol: &InMemVolume, path: &str) -> Vec<u8> {
        let handle = vol.open(Path::new(path)).unwrap();
        let size = vol.stat(Path::new(path)).unwrap().size as usize;
        let mut buf = vec![0u8; size];
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
            vol.open(Path::new("/hello.txt")),
            Err(CortexError::NotFound)
        ));
    }

    #[test]
    fn positioned_writes_and_truncate_are_shared() {
        let vol = InMemVolume::new();
        write_file(&vol, "/f", b"world");

        // A second handle sees writes made through the first, and both share the
        // tree's buffer, so `stat` reflects the new size.
        let a = vol.open(Path::new("/f")).unwrap();
        let b = vol.open(Path::new("/f")).unwrap();
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
    fn kind_errors() {
        let vol = InMemVolume::new();
        vol.mkdir(Path::new("/dir")).unwrap();
        write_file(&vol, "/file", b"x");

        assert!(matches!(
            vol.open(Path::new("/dir")),
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
            vol.create(Path::new("/file")),
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
            vol.create(Path::new("/missing/deep")),
            Err(CortexError::NotFound)
        ));
        assert!(matches!(
            vol.mkdir(Path::new("/a/../b")),
            Err(CortexError::InvalidName)
        ));
    }
}
