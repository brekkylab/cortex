use crate::error::{Result, CortexError};
use crate::volume::{Dirent, InMemVolume, Mountable};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

pub struct Workspace {
    /// Mount points keyed by their normalized, root-relative path.
    ///
    /// The empty path is the workspace root and always resolves as the
    /// fallback mount. `PathBuf`'s component-wise `Ord` guarantees that, among
    /// all keys that are a prefix of a request, the longest is also the
    /// lexicographically greatest — so a longest-prefix lookup is a reverse
    /// range scan (see [`Workspace::resolve`]).
    mounts: BTreeMap<PathBuf, Box<dyn Mountable>>,
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            mounts: BTreeMap::from([(
                PathBuf::new(),
                Box::new(InMemVolume::new()) as Box<dyn Mountable>,
            )]),
        }
    }

    /// Builder-style mount that overwrites any volume already at `path`.
    ///
    /// Panics if `path` escapes the workspace root; intended for setup code
    /// where the mount points are known to be valid.
    pub fn try_with_mountable(
        mut self,
        path: impl AsRef<Path>,
        mountable: Box<dyn Mountable>,
    ) -> Result<Self> {
        self.mounts.insert(normalize(path.as_ref())?, mountable);
        Ok(self)
    }

    /// Mount `volume` at `at` (root-relative). Fails if the path escapes the
    /// workspace root or another volume is already mounted there.
    pub fn mount(&mut self, path: impl AsRef<Path>, mountable: Box<dyn Mountable>) -> Result<()> {
        let key = normalize(path.as_ref())?;
        if self.mounts.contains_key(&key) {
            return Err(CortexError::AlreadyExists);
        }
        self.mounts.insert(key, mountable);
        Ok(())
    }

    /// Iterate over every mount point and the volume mounted there, so an
    /// [`Executable`](crate::Executable) can see what the workspace is composed
    /// of. The root mount has an empty path.
    pub fn mounts(&self) -> impl Iterator<Item = (&Path, &dyn Mountable)> {
        self.mounts
            .iter()
            .map(|(path, volume)| (path.as_path(), volume.as_ref()))
    }

    /// Resolve a request path to the volume that owns it (longest-prefix
    /// match) together with the path re-based onto that volume's mount point.
    fn resolve(&self, path: &Path) -> Result<(&dyn Mountable, PathBuf)> {
        let key = normalize(path)?;
        // Every key <= `key`, walked from the greatest downward; the first one
        // that is a prefix of `key` is the longest match. The empty root key is
        // a prefix of everything, so a match always exists.
        let (mount, volume) = self
            .mounts
            .range(..=key.clone())
            .rev()
            .find(|(k, _)| key.starts_with(k))
            .expect("root mount is a prefix of every path");
        let sub = key
            .strip_prefix(mount)
            .expect("matched mount is a prefix of the request");
        Ok((volume.as_ref(), sub.to_path_buf()))
    }
}

/// Canonicalize a request into a root-relative path of `Normal` components
/// only. `.` is dropped and `..` pops the previous component; absolute paths
/// and any `..` that would escape the workspace root are rejected.
fn normalize(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(CortexError::InvalidName);
                }
            }
            Component::RootDir | Component::Prefix(_) => return Err(CortexError::InvalidName),
        }
    }
    Ok(out)
}

impl Mountable for Workspace {
    fn list(&self, path: &Path) -> Result<Vec<Dirent>> {
        let (volume, sub) = self.resolve(path)?;
        volume.list(&sub)
    }

    fn mkdir(&self, path: &Path) -> Result<()> {
        let (volume, sub) = self.resolve(path)?;
        volume.mkdir(&sub)
    }

    fn unlink(&self, path: &Path) -> Result<()> {
        let (volume, sub) = self.resolve(path)?;
        volume.unlink(&sub)
    }

    fn read(&self, path: &Path) -> Result<Vec<u8>> {
        let (volume, sub) = self.resolve(path)?;
        volume.read(&sub)
    }

    fn write(&self, path: &Path, data: &[u8]) -> Result<()> {
        let (volume, sub) = self.resolve(path)?;
        volume.write(&sub, data)
    }
}
