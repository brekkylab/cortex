//! A namespace that stitches several backends into one filesystem.
//!
//! A [`Workspace`] is a longest-prefix mount table: each backend is registered
//! at a root-relative path and serves every request that falls under it, with
//! the path re-based onto the backend's own root. Because the backends have
//! different concrete `Handle` types, they are stored as
//! [`DynMountable`](crate::DynMountable) trait objects.
//!
//! A `Workspace` is itself a [`Mountable`] (its handle is `Box<dyn FileHandle>`),
//! so it can be driven by the same adapters as any single backend — and even
//! mounted inside another workspace.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::{CortexError, Dirent, DynMountable, FileHandle, Mountable, Result, Stat};

/// A longest-prefix mount table over heterogeneous backends.
pub struct Workspace {
    /// Mount points keyed by their normalized, root-relative path.
    ///
    /// `PathBuf`'s component-wise `Ord` guarantees that, among all keys that are
    /// a prefix of a request, the longest is also the lexicographically greatest
    /// — so a longest-prefix lookup is a reverse range scan (see
    /// [`Workspace::resolve`]). A backend mounted at the empty path is the root
    /// and serves anything no deeper mount claims.
    mounts: BTreeMap<PathBuf, Box<dyn DynMountable>>,
}

impl Workspace {
    /// An empty workspace with no mounts. Until something is mounted, every
    /// request resolves to [`CortexError::NotFound`]; mount a backend at the
    /// empty path to give it a root.
    pub fn new() -> Self {
        Self {
            mounts: BTreeMap::new(),
        }
    }

    /// Builder-style mount that overwrites any backend already at `path`.
    ///
    /// Fails only if `path` escapes the workspace root.
    pub fn try_with_mount<M>(mut self, path: impl AsRef<Path>, backend: M) -> Result<Self>
    where
        M: Mountable + 'static,
        M::Handle: 'static,
    {
        self.mounts
            .insert(normalize(path.as_ref())?, Box::new(backend));
        Ok(self)
    }

    /// Mount `backend` at `path` (root-relative). Fails if the path escapes the
    /// workspace root or another backend is already mounted there.
    pub fn mount<M>(&mut self, path: impl AsRef<Path>, backend: M) -> Result<()>
    where
        M: Mountable + 'static,
        M::Handle: 'static,
    {
        let key = normalize(path.as_ref())?;
        if self.mounts.contains_key(&key) {
            return Err(CortexError::AlreadyExists);
        }
        self.mounts.insert(key, Box::new(backend));
        Ok(())
    }

    /// Remove the mount registered at `path`, returning the detached backend.
    pub fn unmount(&mut self, path: impl AsRef<Path>) -> Result<Box<dyn DynMountable>> {
        let key = normalize(path.as_ref())?;
        self.mounts.remove(&key).ok_or(CortexError::NotFound)
    }

    /// Resolve a request path to the backend that owns it (longest-prefix
    /// match) together with the path re-based onto that backend's mount point.
    fn resolve(&self, path: &Path) -> Result<(&dyn DynMountable, PathBuf)> {
        let key = normalize(path)?;
        // Walk every key <= `key` from the greatest downward; the first that is a
        // prefix of `key` is the longest match. `NotFound` if nothing claims it.
        let (mount, backend) = self
            .mounts
            .range(..=key.clone())
            .rev()
            .find(|(k, _)| key.starts_with(k))
            .ok_or(CortexError::NotFound)?;
        let sub = key
            .strip_prefix(mount)
            .expect("matched mount is a prefix of the request");
        Ok((backend.as_ref(), sub.to_path_buf()))
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonicalize a request into a root-relative path of `Normal` components
/// only. `.` and a leading root are dropped and `..` pops the previous
/// component; any `..` that would escape the workspace root, and OS prefixes,
/// are rejected.
fn normalize(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(CortexError::InvalidName);
                }
            }
            Component::Prefix(_) => return Err(CortexError::InvalidName),
        }
    }
    Ok(out)
}

impl Mountable for Workspace {
    type Handle = Box<dyn FileHandle>;

    fn stat(&self, path: &Path) -> Result<Stat> {
        let (backend, sub) = self.resolve(path)?;
        backend.stat(&sub)
    }

    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let (backend, sub) = self.resolve(path)?;
        backend.list(&sub)
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        let (backend, sub) = self.resolve(path)?;
        backend.mkdir(&sub)
    }

    fn unlink(&self, path: &Path) -> Result<()> {
        let (backend, sub) = self.resolve(path)?;
        backend.unlink(&sub)
    }

    fn open(&self, path: &Path) -> Result<Self::Handle> {
        let (backend, sub) = self.resolve(path)?;
        backend.open(&sub)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DirentKind, FileExt, PassthroughVolume};
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "cortex-workspace-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn longest_prefix_routing() {
        // Two on-disk backends: a root, and a deeper one mounted at `data`.
        let root_dir = scratch("root");
        fs::write(root_dir.join("top.txt"), b"root").unwrap();

        let data_dir = scratch("data");
        fs::write(data_dir.join("inner.txt"), b"inner").unwrap();

        let ws = Workspace::new()
            .try_with_mount("", PassthroughVolume::new(&root_dir))
            .unwrap()
            .try_with_mount("data", PassthroughVolume::new(&data_dir))
            .unwrap();

        // Both `Mountable` and `DynMountable` (blanket impl) are in scope, so
        // calls to shared method names must name the trait explicitly.

        // `top.txt` is served by the root backend.
        assert_eq!(
            Mountable::stat(&ws, Path::new("top.txt")).unwrap().kind,
            DirentKind::File
        );
        let h = Mountable::open(&ws, Path::new("top.txt")).unwrap();
        let mut buf = [0u8; 4];
        h.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"root");

        // `data/inner.txt` is routed to the deeper mount, re-based to `inner.txt`.
        let h = Mountable::open(&ws, Path::new("data/inner.txt")).unwrap();
        let mut buf = [0u8; 5];
        h.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"inner");

        assert_eq!(
            Mountable::list(&ws, Path::new("data")).unwrap().len(),
            1,
            "the deeper mount only sees its own single file"
        );

        fs::remove_dir_all(&root_dir).unwrap();
        fs::remove_dir_all(&data_dir).unwrap();
    }

    #[test]
    fn unmounted_paths_are_not_found() {
        let ws = Workspace::new();
        assert!(matches!(
            Mountable::stat(&ws, Path::new("anything")),
            Err(CortexError::NotFound)
        ));
    }

    #[test]
    fn mount_rejects_duplicates_and_escapes() {
        let dir = scratch("dup");
        let mut ws = Workspace::new();
        ws.mount("m", PassthroughVolume::new(&dir)).unwrap();
        assert!(matches!(
            ws.mount("m", PassthroughVolume::new(&dir)),
            Err(CortexError::AlreadyExists)
        ));
        assert!(matches!(
            ws.mount("../escape", PassthroughVolume::new(&dir)),
            Err(CortexError::InvalidName)
        ));
        fs::remove_dir_all(&dir).unwrap();
    }
}
