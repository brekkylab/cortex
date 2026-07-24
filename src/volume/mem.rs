//! An in-memory [`Mountable`] backend.
//!
//! The whole tree lives behind `Rc<RefCell<..>>` links so that every operation
//! shares one store through `&self`. It is single-threaded (not `Send`/`Sync`)
//! — enough for tests, scratch space, and prototyping.
//!
//! In addition to the plain tree, an `InMemVolume` can have other [`Mountable`]s
//! [mounted][`InMemVolume::mount`] at specific directories. Any path that falls
//! under a mount point is transparently delegated to the mounted volume, with
//! the path rewritten to be relative to the mount root.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use crate::{Dirent, Result, Mountable, CortexError};

type Link = Rc<RefCell<Node>>;

enum Node {
    Dir { children: HashMap<String, Link> },
    File { data: Vec<u8> },
}

impl Node {
    fn new_dir() -> Link {
        Rc::new(RefCell::new(Node::Dir {
            children: HashMap::new(),
        }))
    }

    fn new_file(data: Vec<u8>) -> Link {
        Rc::new(RefCell::new(Node::File { data }))
    }
}

/// An in-memory volume: a directory tree plus any sub-volumes mounted into it.
pub struct InMemVolume {
    root: Link,
    /// Mount points keyed by their normalized path components. The longest
    /// matching prefix wins when a request path could belong to more than one.
    mounts: RefCell<HashMap<Vec<String>, Box<dyn Mountable>>>,
}

impl InMemVolume {
    /// Create a fresh, empty in-memory volume.
    pub fn new() -> Self {
        InMemVolume {
            root: Node::new_dir(),
            mounts: RefCell::new(HashMap::new()),
        }
    }

    /// Mount `volume` at `at`, so that every path under `at` is served by
    /// `volume` (with the path made relative to `at`).
    ///
    /// The parent of `at` must exist as a directory. `at` itself may already be
    /// a local directory (its contents are shadowed while mounted) but must not
    /// be a file, and must not already have a volume mounted on it. The root
    /// cannot be mounted over.
    pub fn mount(&self, at: impl AsRef<Path>, volume: Box<dyn Mountable>) -> Result<()> {
        let comps = components(at.as_ref())?;
        let (parent, name) = split_last(&comps)?;

        // Parent must exist as a directory; the slot must not hold a file.
        let dir = self.navigate(parent)?;
        match &*dir.borrow() {
            Node::Dir { children } => {
                if let Some(existing) = children.get(name) {
                    if matches!(&*existing.borrow(), Node::File { .. }) {
                        return Err(CortexError::NotADirectory);
                    }
                }
            }
            Node::File { .. } => return Err(CortexError::NotADirectory),
        }

        let mut mounts = self.mounts.borrow_mut();
        if mounts.contains_key(&comps) {
            return Err(CortexError::AlreadyExists);
        }
        mounts.insert(comps, volume);
        Ok(())
    }

    /// Remove the mount registered at `at`, returning the detached volume.
    pub fn unmount(&self, at: impl AsRef<Path>) -> Result<Box<dyn Mountable>> {
        let comps = components(at.as_ref())?;
        self.mounts
            .borrow_mut()
            .remove(&comps)
            .ok_or(CortexError::NotFound)
    }

    /// Walk from the root to the node addressed by `comps`. Every intermediate
    /// component must be a directory.
    fn navigate(&self, comps: &[String]) -> Result<Link> {
        let mut cur = self.root.clone();
        for name in comps {
            let next = match &*cur.borrow() {
                Node::Dir { children } => {
                    children.get(name).cloned().ok_or(CortexError::NotFound)?
                }
                Node::File { .. } => return Err(CortexError::NotADirectory),
            };
            cur = next;
        }
        Ok(cur)
    }

    /// Route an operation on `comps` to the innermost mounted volume that owns
    /// it, or fall back to the local tree.
    ///
    /// `on_mount` receives the mounted volume and the path relative to the
    /// mount root; `on_local` runs against this volume's own tree.
    fn dispatch<T>(
        &self,
        comps: &[String],
        on_mount: impl FnOnce(&dyn Mountable, &Path) -> Result<T>,
        on_local: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mounts = self.mounts.borrow();
        let best = mounts
            .iter()
            .filter(|(k, _)| comps.len() >= k.len() && comps[..k.len()] == k[..])
            .max_by_key(|(k, _)| k.len());
        match best {
            Some((k, volume)) => {
                let mut rel = PathBuf::from("/");
                rel.extend(&comps[k.len()..]);
                on_mount(volume.as_ref(), &rel)
            }
            None => {
                drop(mounts);
                on_local()
            }
        }
    }
}

impl Default for InMemVolume {
    fn default() -> Self {
        Self::new()
    }
}

impl Mountable for InMemVolume {
    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let comps = components(path)?;
        let mut entries = self.dispatch(
            &comps,
            |volume, rel| volume.list(rel),
            || {
                let link = self.navigate(&comps)?;
                match &*link.borrow() {
                    Node::Dir { children } => Ok(children
                        .iter()
                        .map(|(name, child)| match &*child.borrow() {
                            Node::Dir { .. } => Dirent::Dir(name.clone()),
                            Node::File { .. } => Dirent::File(name.clone()),
                        })
                        .collect()),
                    Node::File { .. } => Err(CortexError::NotADirectory),
                }
            },
        )?;

        // Surface mount points as directory entries of their parent.
        let mounts = self.mounts.borrow();
        for key in mounts.keys() {
            if key.len() == comps.len() + 1 && key[..comps.len()] == comps[..] {
                let name = &key[comps.len()];
                if !entries.iter().any(|e| e.name() == name) {
                    entries.push(Dirent::Dir(name.clone()));
                }
            }
        }
        Ok(entries)
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        let comps = components(path)?;
        self.dispatch(
            &comps,
            |volume, rel| volume.mkdir(rel),
            || {
                let (parent, name) = split_last(&comps)?;
                let dir = self.navigate(parent)?;
                let mut node = dir.borrow_mut();
                match &mut *node {
                    Node::Dir { children } => match children.get(name) {
                        Some(existing) => match &*existing.borrow() {
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
            },
        )
    }

    fn unlink(&self, path: &Path) -> Result<()> {
        let comps = components(path)?;
        self.dispatch(
            &comps,
            |volume, rel| volume.unlink(rel),
            || {
                let (parent, name) = split_last(&comps)?;
                let dir = self.navigate(parent)?;
                let mut node = dir.borrow_mut();
                match &mut *node {
                    Node::Dir { children } => {
                        children.remove(name).map(|_| ()).ok_or(CortexError::NotFound)
                    }
                    Node::File { .. } => Err(CortexError::NotADirectory),
                }
            },
        )
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        let comps = components(path)?;
        self.dispatch(
            &comps,
            |volume, rel| volume.read(rel),
            || {
                let link = self.navigate(&comps)?;
                match &*link.borrow() {
                    Node::File { data } => Ok(data.clone()),
                    Node::Dir { .. } => Err(CortexError::IsADirectory),
                }
            },
        )
    }

    fn write(&self, path: &Path, data: &[u8]) -> Result<()> {
        let comps = components(path)?;
        self.dispatch(
            &comps,
            |volume, rel| volume.write(rel, data),
            || {
                let (parent, name) = split_last(&comps)?;
                let dir = self.navigate(parent)?;
                let mut node = dir.borrow_mut();
                match &mut *node {
                    Node::Dir { children } => {
                        if let Some(existing) = children.get(name) {
                            if matches!(&*existing.borrow(), Node::Dir { .. }) {
                                return Err(CortexError::IsADirectory);
                            }
                        }
                        children.insert(name.clone(), Node::new_file(data.to_vec()));
                        Ok(())
                    }
                    Node::File { .. } => Err(CortexError::NotADirectory),
                }
            },
        )
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

    fn names(vol: &dyn Mountable, path: &str) -> Vec<String> {
        let mut names: Vec<_> = vol
            .list(Path::new(path))
            .unwrap()
            .iter()
            .map(|e| e.name().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_read_list_unlink() {
        let vol = InMemVolume::new();
        vol.mkdir(Path::new("/sub")).unwrap();
        vol.write(Path::new("/hello.txt"), b"world").unwrap();
        vol.write(Path::new("/sub/inner"), b"hi").unwrap();

        assert_eq!(vol.read(Path::new("/hello.txt")).unwrap(), b"world");
        assert_eq!(vol.read(Path::new("/sub/inner")).unwrap(), b"hi");
        assert_eq!(names(&vol, "/"), vec!["hello.txt", "sub"]);
        assert_eq!(names(&vol, "/sub"), vec!["inner"]);

        vol.unlink(Path::new("/hello.txt")).unwrap();
        assert!(matches!(
            vol.read(Path::new("/hello.txt")),
            Err(CortexError::NotFound)
        ));
    }

    #[test]
    fn kind_errors() {
        let vol = InMemVolume::new();
        vol.mkdir(Path::new("/dir")).unwrap();
        vol.write(Path::new("/file"), b"x").unwrap();

        assert!(matches!(
            vol.read(Path::new("/dir")),
            Err(CortexError::IsADirectory)
        ));
        assert!(matches!(
            vol.write(Path::new("/dir"), b"y"),
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
    }

    #[test]
    fn invalid_paths_rejected() {
        let vol = InMemVolume::new();
        assert!(matches!(
            vol.mkdir(Path::new("/a/../b")),
            Err(CortexError::InvalidName)
        ));
    }

    #[test]
    fn mounts_delegate_operations() {
        let root = InMemVolume::new();
        root.mkdir(Path::new("/mnt")).unwrap();

        let inner = InMemVolume::new();
        inner.write(Path::new("/data.txt"), b"from inner").unwrap();
        root.mount("/mnt", Box::new(inner)).unwrap();

        // Reads and listings under the mount hit the mounted volume.
        assert_eq!(root.read(Path::new("/mnt/data.txt")).unwrap(), b"from inner");
        assert_eq!(names(&root, "/mnt"), vec!["data.txt"]);

        // Writes through the mount land in the mounted volume.
        root.write(Path::new("/mnt/new.txt"), b"added").unwrap();
        assert_eq!(names(&root, "/mnt"), vec!["data.txt", "new.txt"]);

        // The mount point shows up when listing its parent.
        assert_eq!(names(&root, "/"), vec!["mnt"]);
    }

    #[test]
    fn mount_slot_and_parent_validated() {
        let root = InMemVolume::new();
        root.write(Path::new("/afile"), b"x").unwrap();

        // Cannot shadow a file, and the parent must exist.
        assert!(matches!(
            root.mount("/afile", Box::new(InMemVolume::new())),
            Err(CortexError::NotADirectory)
        ));
        assert!(matches!(
            root.mount("/missing/deep", Box::new(InMemVolume::new())),
            Err(CortexError::NotFound)
        ));

        // Mounting twice at the same point is rejected.
        root.mkdir(Path::new("/m")).unwrap();
        root.mount("/m", Box::new(InMemVolume::new())).unwrap();
        assert!(matches!(
            root.mount("/m", Box::new(InMemVolume::new())),
            Err(CortexError::AlreadyExists)
        ));
    }

    #[test]
    fn nested_mount_prefers_longest_prefix() {
        let root = InMemVolume::new();
        root.mkdir(Path::new("/a")).unwrap();

        let outer = InMemVolume::new();
        outer.write(Path::new("/x"), b"outer").unwrap();
        root.mount("/a", Box::new(outer)).unwrap();

        let inner = InMemVolume::new();
        inner.write(Path::new("/y"), b"inner").unwrap();
        root.mount("/a/b", Box::new(inner)).unwrap();

        assert_eq!(root.read(Path::new("/a/x")).unwrap(), b"outer");
        assert_eq!(root.read(Path::new("/a/b/y")).unwrap(), b"inner");
    }

    #[test]
    fn unmount_detaches() {
        let root = InMemVolume::new();
        root.mkdir(Path::new("/m")).unwrap();
        root.mount("/m", Box::new(InMemVolume::new())).unwrap();
        root.write(Path::new("/m/f"), b"z").unwrap();

        root.unmount("/m").unwrap();
        assert!(matches!(root.unmount("/m"), Err(CortexError::NotFound)));
        // With the mount gone, `/m` is just an empty local directory again.
        assert_eq!(names(&root, "/m"), Vec::<String>::new());
    }
}
