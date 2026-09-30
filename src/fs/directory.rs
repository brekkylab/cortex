//! A tree built from files held in memory and host directories grafted into it.

use std::{
    collections::BTreeMap,
    io,
    ops::Bound::{Excluded, Unbounded},
    path::{Component, Path, PathBuf},
};

use crate::{
    BoxFuture,
    fs::{Dirent, DirentKind, FileSystem, InMemFs, PassthroughFs, Stat},
};

/// A tree assembled from in-memory files and host directories, served as one namespace.
///
/// An [`InMemFs`] root under a longest-prefix mount table: each [`PassthroughFs`] serves every
/// request under its root-relative mount point, re-based onto its own root; everything else is
/// the in-memory tree's. Itself a [`FileSystem`], so bindings drive it like any single store.
///
/// Builds a session's *context* (as opposed to its rootfs): many sources in one namespace,
/// where a session's mounts are separate namespaces.
///
/// The two kinds of content do not nest: files go to the in-memory tree only, so a path under a
/// mount is refused rather than written to the host, and mounts are disjoint, so a host
/// directory never hides another or files already added.
pub struct Directory {
    /// Everything no mount claims, including the directories leading to each mount point, which
    /// [`mount`](Self::mount) creates so the table never has to invent them.
    root: InMemFs,

    /// Mount points keyed by their normalized, root-relative path. Never the empty path: the
    /// root is [`root`](Self::root)'s.
    mounts: BTreeMap<PathBuf, PassthroughFs>,
}

impl Directory {
    /// A tree with an empty [`InMemFs`] at its root and nothing mounted.
    pub fn new() -> Self {
        Directory {
            root: InMemFs::new(),
            mounts: BTreeMap::new(),
        }
    }

    /// Builder-style [`add_file`](Self::add_file), failing as it does.
    pub fn with_file(mut self, path: impl AsRef<Path>, content: impl io::Read) -> io::Result<Self> {
        self.add_file(path, content)?;
        Ok(self)
    }

    /// Builder-style [`mount`](Self::mount), failing as it does.
    pub fn with_mount(
        mut self,
        path: impl AsRef<Path>,
        host_dir: impl Into<PathBuf>,
    ) -> io::Result<Self> {
        self.mount(path, host_dir)?;
        Ok(self)
    }

    /// Put a file at `path` holding everything `content` yields, making the directories on
    /// the way and replacing a file already there.
    ///
    /// A `path` under a mount is [`InvalidInput`](io::ErrorKind::InvalidInput), not a write to
    /// the host directory.
    pub fn add_file(&mut self, path: impl AsRef<Path>, content: impl io::Read) -> io::Result<()> {
        let key = self.in_memory_key(path.as_ref())?;
        self.root.put_file(&key, content)
    }

    /// Remove an in-memory file, whether added or made by a command since. Refused with
    /// [`InvalidInput`](io::ErrorKind::InvalidInput) under a mount.
    pub fn remove_file(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let key = self.in_memory_key(path.as_ref())?;
        self.root.remove_file(&key)
    }

    /// Serve the host directory `host_dir` at `path` (root-relative), making the directories
    /// that lead to it.
    ///
    /// Refused, since each would hide what is there, with
    /// [`InvalidInput`](io::ErrorKind::InvalidInput) at the root, inside or above another mount,
    /// and with [`AlreadyExists`](io::ErrorKind::AlreadyExists) where the in-memory tree has an
    /// entry. `host_dir` is not checked; a missing one fails at first use.
    ///
    /// Registers in this tree's table only; the OS sees nothing until a binding yields a
    /// [`Mount`](crate::fs::Mount).
    pub fn mount(
        &mut self,
        path: impl AsRef<Path>,
        host_dir: impl Into<PathBuf>,
    ) -> io::Result<()> {
        let key = mount_key(path.as_ref())?;
        if key.as_os_str().is_empty() || self.mount_for(&key).is_some() || self.spans_mounts(&key) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if self.root.contains(&key)? {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let parent = key.parent().expect("a non-empty key has a parent");
        self.root.mkdir_all(parent)?;
        self.mounts.insert(key, PassthroughFs::new(host_dir));
        Ok(())
    }

    /// Remove the mount registered at `path`. The directories [`mount`](Self::mount) made on
    /// the way to it stay, empty.
    pub fn unmount(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let key = mount_key(path.as_ref())?;
        self.mounts.remove(&key).map(drop).ok_or_else(not_found)
    }

    /// `path` normalized, provided it belongs to the in-memory tree.
    fn in_memory_key(&self, path: &Path) -> io::Result<PathBuf> {
        let key = normalize(path)?;
        if self.mount_for(&key).is_some() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(key)
    }

    /// The store that owns `key` (longest-prefix match), with `key` re-based onto that store's
    /// root. Anything no mount claims is the in-memory tree's.
    ///
    /// `key` must already be normalized: [`Posix`](crate::fs::Posix) passes `/`-rooted paths,
    /// and a stray `RootDir` component would miss every mount.
    fn route(&self, key: &Path) -> (&dyn FileSystem, PathBuf) {
        match self.mount_for(key) {
            Some(mount) => {
                let sub = key
                    .strip_prefix(mount)
                    .expect("matched mount is a prefix of the request");
                (&self.mounts[mount], sub.to_path_buf())
            }
            None => (&self.root, key.to_path_buf()),
        }
    }

    /// [`route`](Self::route) for the data plane, where a directory on the way to a mount is
    /// a directory whatever is asked of it.
    fn route_file(&self, key: &Path) -> io::Result<(&dyn FileSystem, PathBuf)> {
        self.guard_spans_mounts(key, io::ErrorKind::IsADirectory)?;
        Ok(self.route(key))
    }

    /// The mount point that owns `key`, if a mount does.
    ///
    /// Exposed separately so a move can compare *which* mount owns each path, rather than
    /// comparing resolved stores by fragile pointer identity.
    fn mount_for(&self, key: &Path) -> Option<&Path> {
        // Among keys <= `key`, the greatest that is a prefix is the longest match.
        self.mounts
            .range(..=key.to_path_buf())
            .rev()
            .find(|(mount, _)| key.starts_with(mount))
            .map(|(mount, _)| mount.as_path())
    }

    /// Every mount *strictly* below `prefix`.
    ///
    /// One contiguous run of the map: component-wise `Ord` puts `["a"] < ["a","b"]` and
    /// `["a","z"] < ["b"]`, so keys sharing a prefix are adjacent and a sibling like `ab` cannot
    /// interleave. `Excluded` keeps a mount from being below itself.
    fn descendant_mounts<'a>(&'a self, prefix: &'a Path) -> impl Iterator<Item = &'a PathBuf> {
        self.mounts
            .range((Excluded(prefix.to_path_buf()), Unbounded))
            .map(|(k, _)| k)
            .take_while(move |k| k.starts_with(prefix))
    }

    /// The child names of `prefix` that come from the mount table, each flagged with whether a
    /// mount sits at *exactly* that path.
    ///
    /// The flag settles name collisions with the in-memory tree: a mount point shadows the
    /// tree's entry, while a name leading to a deeper mount keeps it.
    fn mount_children(&self, prefix: &Path) -> BTreeMap<String, bool> {
        let mut out = BTreeMap::new();
        for mount in self.descendant_mounts(prefix) {
            let rest = mount
                .strip_prefix(prefix)
                .expect("the range only yields keys prefixed by `prefix`");
            let mut components = rest.components();
            let Some(first) = components.next() else {
                continue;
            };
            let name = first.as_os_str().to_string_lossy().into_owned();
            // One component left over means the mount is at this child itself.
            let exact = components.next().is_none();
            *out.entry(name).or_insert(false) |= exact;
        }
        out
    }

    /// Whether `prefix` is a directory on the way to some mount.
    fn spans_mounts(&self, prefix: &Path) -> bool {
        self.descendant_mounts(prefix).next().is_some()
    }

    /// Whether `key` belongs to the mount table rather than to a store — a mount point itself,
    /// or a directory on the way to one.
    ///
    /// Neither may be renamed: that would rewrite the mount table through a file operation.
    fn is_mount_table_owned(&self, key: &Path) -> bool {
        self.mounts.contains_key(key) || self.spans_mounts(key)
    }

    /// Refuse a mutation aimed at a directory on the way to a mount.
    ///
    /// Checked *before* the store: the in-memory tree holds only the directory, not what is
    /// mounted below, so it would let `rmdir` remove a directory the table needs and detach
    /// the mount.
    fn guard_spans_mounts(&self, key: &Path, refusal: io::ErrorKind) -> io::Result<()> {
        if self.spans_mounts(key) {
            Err(refusal.into())
        } else {
            Ok(())
        }
    }
}

impl Default for Directory {
    fn default() -> Self {
        Self::new()
    }
}

fn not_found() -> io::Error {
    io::ErrorKind::NotFound.into()
}

/// Normalize a path for use as a mount key, additionally refusing components that are not valid
/// UTF-8.
///
/// Request paths may be non-UTF-8, but a mount point's name is listed through the `String`
/// [`Dirent::name`], and a lossy name would list an entry whose `lookup` then fails.
fn mount_key(path: &Path) -> io::Result<PathBuf> {
    let key = normalize(path)?;
    if key
        .components()
        .any(|component| component.as_os_str().to_str().is_none())
    {
        return Err(io::ErrorKind::InvalidFilename.into());
    }
    Ok(key)
}

/// Canonicalize a request into a root-relative path of `Normal` components only. `.` and a
/// leading root are dropped and `..` pops the previous component; any `..` that would escape the
/// tree's root, and OS prefixes, are rejected.
fn normalize(path: &Path) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(io::ErrorKind::InvalidFilename.into());
                }
            }
            Component::Prefix(_) => return Err(io::ErrorKind::InvalidFilename.into()),
        }
    }
    Ok(out)
}

impl FileSystem for Directory {
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            let key = normalize(path)?;
            let (store, sub) = self.route(&key);
            store.stat(&sub).await
        })
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        Box::pin(async move {
            let key = normalize(path)?;
            let children = self.mount_children(&key);
            let (store, sub) = self.route(&key);
            let entries = store.list(&sub).await?;

            // Split off entries the mount table also names, so each name is emitted once.
            let (mut named_by_mounts, store_only): (Vec<_>, Vec<_>) = entries
                .into_iter()
                .partition(|entry| children.contains_key(&entry.name));
            let mut claimed: BTreeMap<String, Dirent> = named_by_mounts
                .drain(..)
                .map(|entry| (entry.name.clone(), entry))
                .collect();

            // Mount-derived names go first, sorted, so their positions depend only on the table.
            // `readdir` resumes at a position in this list, so with them last a new store file
            // would shift every mount point and drop one from an in-progress listing.
            let mut out = Vec::with_capacity(children.len() + store_only.len());
            for (name, mounted_here) in children {
                let store_entry = claimed.remove(&name);
                out.push(match (mounted_here, store_entry) {
                    // A mount point shadows the tree's entry. No stat: the kernel's `lookup`
                    // for metadata routes to the mount, so the answers cannot disagree.
                    (true, _) => Dirent::new(name, DirentKind::Dir),
                    // On the way to a deeper mount: keep the tree's directory for its metadata.
                    (false, Some(entry)) => entry,
                    (false, None) => Dirent::new(name, DirentKind::Dir),
                });
            }
            out.extend(store_only);
            Ok(out)
        })
    }

    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            let key = normalize(path)?;
            let (store, sub) = self.route_file(&key)?;
            store.read_at(&sub, buf, offset).await
        })
    }

    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            let key = normalize(path)?;
            // The name is taken by a directory the mount table needs; an exclusive create
            // answers `AlreadyExists`.
            self.guard_spans_mounts(&key, io::ErrorKind::AlreadyExists)?;
            let (store, sub) = self.route(&key);
            store.create(&sub).await
        })
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async move {
            let key = normalize(path)?;
            self.guard_spans_mounts(&key, io::ErrorKind::AlreadyExists)?;
            let (store, sub) = self.route(&key);
            store.mkdir(&sub).await
        })
    }

    fn unlink<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let key = normalize(path)?;
            self.guard_spans_mounts(&key, io::ErrorKind::IsADirectory)?;
            let (store, sub) = self.route(&key);
            store.unlink(&sub).await
        })
    }

    fn rmdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let key = normalize(path)?;
            // Not empty: it holds mount points, which the filesystem may not remove.
            self.guard_spans_mounts(&key, io::ErrorKind::DirectoryNotEmpty)?;
            let (store, sub) = self.route(&key);
            store.rmdir(&sub).await
        })
    }

    fn write_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            let key = normalize(path)?;
            let (store, sub) = self.route_file(&key)?;
            store.write_at(&sub, buf, offset).await
        })
    }

    fn truncate<'a>(&'a self, path: &'a Path, size: u64) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let key = normalize(path)?;
            let (store, sub) = self.route_file(&key)?;
            store.truncate(&sub, size).await
        })
    }

    fn flush<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let key = normalize(path)?;
            let (store, sub) = self.route_file(&key)?;
            store.flush(&sub).await
        })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let (from_key, to_key) = (normalize(from)?, normalize(to)?);

            // Checked before the stores: a refusal must leave the tree untouched, and a store
            // asked about its own root would answer arbitrarily.
            if self.is_mount_table_owned(&from_key) || self.is_mount_table_owned(&to_key) {
                return Err(io::ErrorKind::ReadOnlyFilesystem.into());
            }

            // `EXDEV` makes `mv` copy then delete; copying here could not be atomic on
            // partial failure, which `mv` already handles.
            if self.mount_for(&from_key) != self.mount_for(&to_key) {
                return Err(io::ErrorKind::CrossesDevices.into());
            }

            let (store, from_sub) = self.route(&from_key);
            let (_, to_sub) = self.route(&to_key);
            store.rename(&from_sub, &to_sub).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(entries: Vec<Dirent>) -> Vec<String> {
        let mut names: Vec<_> = entries.into_iter().map(|e| e.name).collect();
        names.sort();
        names
    }

    async fn read_all(dir: &Directory, path: &str) -> String {
        let mut buf = vec![0; 64];
        let n = dir.read_at(Path::new(path), &mut buf, 0).await.unwrap();
        String::from_utf8(buf[..n].to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_fresh_directory_is_a_writable_empty_tree() {
        let dir = Directory::new();
        assert!(dir.list(Path::new("")).await.unwrap().is_empty());
        dir.create(Path::new("scratch.txt")).await.unwrap();
        dir.write_at(Path::new("scratch.txt"), b"hi", 0)
            .await
            .unwrap();
        assert_eq!(read_all(&dir, "scratch.txt").await, "hi");
    }

    #[tokio::test]
    async fn an_added_file_makes_its_parents_and_replaces_an_old_one() {
        let mut dir = Directory::new();
        dir.add_file("a/b/c.txt", "one".as_bytes()).unwrap();
        dir.add_file("a/b/c.txt", "two".as_bytes()).unwrap();
        assert_eq!(read_all(&dir, "a/b/c.txt").await, "two");
        assert_eq!(
            dir.add_file("a/b", "".as_bytes()).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );

        dir.remove_file("a/b/c.txt").unwrap();
        assert!(dir.list(Path::new("a/b")).await.unwrap().is_empty());
        assert_eq!(
            dir.remove_file("a/b/c.txt").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn a_mount_serves_its_host_directory_beside_the_in_memory_files() {
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("on-disk.txt"), "disk").unwrap();

        let mut dir = Directory::new();
        dir.add_file("readme.md", "memory".as_bytes()).unwrap();
        dir.mount("deep/project", host.path()).unwrap();

        assert_eq!(
            names(dir.list(Path::new("")).await.unwrap()),
            ["deep", "readme.md"]
        );
        assert_eq!(
            names(dir.list(Path::new("deep")).await.unwrap()),
            ["project"]
        );
        assert_eq!(read_all(&dir, "deep/project/on-disk.txt").await, "disk");
        assert_eq!(read_all(&dir, "readme.md").await, "memory");

        // A directory on the way to a mount is the table's.
        assert_eq!(
            dir.rmdir(Path::new("deep")).await.unwrap_err().kind(),
            io::ErrorKind::DirectoryNotEmpty
        );
        // Memory to mount crosses stores.
        assert_eq!(
            dir.rename(Path::new("readme.md"), Path::new("deep/project/readme.md"))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::CrossesDevices
        );
    }

    #[tokio::test]
    async fn the_builder_assembles_the_same_tree() {
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("on-disk.txt"), "disk").unwrap();

        let dir = Directory::new()
            .with_file("readme.md", "memory".as_bytes())
            .unwrap()
            .with_mount("project", host.path())
            .unwrap();
        assert_eq!(read_all(&dir, "readme.md").await, "memory");
        assert_eq!(read_all(&dir, "project/on-disk.txt").await, "disk");

        assert_eq!(
            dir.with_file("project/x", "".as_bytes())
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn files_are_not_added_or_removed_under_a_mount() {
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("on-disk.txt"), "disk").unwrap();
        let mut dir = Directory::new();
        dir.mount("project", host.path()).unwrap();

        for path in ["project", "project/new.txt", "/project/sub/new.txt"] {
            assert_eq!(
                dir.add_file(path, "x".as_bytes()).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{path}"
            );
        }
        assert_eq!(
            dir.remove_file("project/on-disk.txt").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(host.path().join("on-disk.txt").exists());
        assert!(!host.path().join("new.txt").exists());
    }

    #[test]
    fn a_mount_hides_nothing() {
        let mut dir = Directory::new();
        dir.add_file("taken.txt", "".as_bytes()).unwrap();
        dir.mount("a/b", "/nonexistent").unwrap();

        let refused = |dir: &mut Directory, path: &str| dir.mount(path, "/x").unwrap_err().kind();
        assert_eq!(refused(&mut dir, ""), io::ErrorKind::InvalidInput);
        assert_eq!(refused(&mut dir, "a/b"), io::ErrorKind::InvalidInput);
        assert_eq!(refused(&mut dir, "a/b/c"), io::ErrorKind::InvalidInput);
        assert_eq!(refused(&mut dir, "a"), io::ErrorKind::InvalidInput);
        assert_eq!(refused(&mut dir, "taken.txt"), io::ErrorKind::AlreadyExists);
        assert_eq!(
            refused(&mut dir, "taken.txt/x"),
            io::ErrorKind::NotADirectory
        );

        dir.unmount("a/b").unwrap();
        dir.mount("a/b/c", "/x").unwrap();
        assert_eq!(
            dir.unmount("a/b").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
