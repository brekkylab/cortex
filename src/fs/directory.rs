//! A tree built from files held in memory and host directories grafted into it.
//!
//! A [`Directory`] is an [`InMemFs`] at the root with a longest-prefix mount table over it:
//! each [`PassthroughFs`] is registered at a root-relative path and serves every request that
//! falls under it, with the path re-based onto its own root. Everything no mount claims is the
//! in-memory tree's.
//!
//! It is itself a [`FileSystem`], so it can be driven by the same bindings as any single
//! store.
//!
//! **The `mount` here is the table's, not the operating system's.**
//! [`mount`](Directory::mount) grafts a host directory onto a path in *this* tree and nothing
//! outside the process learns of it. Putting the result where a kernel can see it is
//! [`Mount`](crate::fs::Mount), which is what a binding hands back — so a `Directory` with ten
//! mounts in it may still be mounted nowhere at all.

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

/// A public API for using virtx's filesystem.
///
/// A *context* is a tree a session is given to work in, as against its rootfs — the system's
/// own tree — and this is what one is assembled out of: files the caller hands over, kept in
/// memory, and host directories placed beside them, all behind one
/// [`mount`](crate::console::ConsoleClientBuilder::mount).
///
/// Which is a different question from the one the console protocol answers by taking a list
/// of mounts, and both answers stand: this composes many sources into *one* namespace a
/// command walks, where a session's mounts are separate namespaces the caller places itself.
/// A tree assembled here is one entry in that list, whatever the caller means it for.
///
/// The two kinds of content do not nest. A file is added to the in-memory tree only, so a
/// path under a mount is refused rather than written through to the host; and mounts are
/// disjoint, so a host directory never hides another one or the files already added.
pub struct Directory {
    /// Where everything no mount claims lives, including the directories that lead to each
    /// mount point — [`mount`](Self::mount) makes those, so every path above a mount is a
    /// real directory here rather than one the table has to invent.
    root: InMemFs,

    /// Mount points keyed by their normalized, root-relative path. Never the empty path: the
    /// root is [`root`](Self::root)'s.
    ///
    /// `PathBuf`'s component-wise `Ord` guarantees that, among all keys that are a prefix of a
    /// request, the longest is also the lexicographically greatest — so a longest-prefix lookup
    /// is a reverse range scan (see [`Directory::mount_for`]), and all keys sharing a prefix
    /// form one contiguous run (see [`Directory::descendant_mounts`]).
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
    /// The file lives in memory, so `path` may not fall under a mount — that is
    /// [`InvalidInput`](io::ErrorKind::InvalidInput), not a write to the host directory.
    pub fn add_file(&mut self, path: impl AsRef<Path>, content: impl io::Read) -> io::Result<()> {
        let key = self.in_memory_key(path.as_ref())?;
        self.root.put_file(&key, content)
    }

    /// Remove a file [`add_file`](Self::add_file) put there, or one a command made since.
    /// Refused under a mount, as `add_file` is.
    pub fn remove_file(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let key = self.in_memory_key(path.as_ref())?;
        self.root.remove_file(&key)
    }

    /// Serve the host directory `host_dir` at `path` (root-relative), making the directories
    /// that lead to it.
    ///
    /// Refused with [`InvalidInput`](io::ErrorKind::InvalidInput) at the root, inside another
    /// mount or above one, and with [`AlreadyExists`](io::ErrorKind::AlreadyExists) where the
    /// in-memory tree already has something — each of those would hide what is there.
    /// `host_dir` is not checked: like [`PassthroughFs::new`], a missing directory fails at
    /// first use.
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
    /// `key` must already be normalized — the caller does that once and reuses it for the
    /// mount-table queries alongside this one. That is a correctness requirement, not a
    /// convenience: [`Posix`](crate::fs::Posix) walks the tree as `/`-rooted paths, so a stray
    /// `RootDir` component would miss every mount.
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
    /// Separate from [`route`](Self::route) because a move has to compare *which* mount each of
    /// its two paths belongs to. Comparing the resolved stores instead would work by pointer
    /// identity, which is a far more fragile thing to rest a correctness decision on.
    fn mount_for(&self, key: &Path) -> Option<&Path> {
        // Walk every key <= `key` from the greatest downward; the first that is a prefix of
        // `key` is the longest match.
        self.mounts
            .range(..=key.to_path_buf())
            .rev()
            .find(|(mount, _)| key.starts_with(mount))
            .map(|(mount, _)| mount.as_path())
    }

    /// Every mount *strictly* below `prefix`.
    ///
    /// One contiguous run of the map, not a scan: component-wise `Ord` puts `["a"] <
    /// ["a","b"]` (equal prefix, shorter first) and `["a","z"] < ["b"]` (they diverge at the
    /// first component), so keys sharing a prefix are adjacent and a sibling like `ab` cannot
    /// interleave. The bound is `Excluded`, so a mount is not below itself.
    fn descendant_mounts<'a>(&'a self, prefix: &'a Path) -> impl Iterator<Item = &'a PathBuf> {
        self.mounts
            .range((Excluded(prefix.to_path_buf()), Unbounded))
            .map(|(k, _)| k)
            .take_while(move |k| k.starts_with(prefix))
    }

    /// The child names of `prefix` that come from the mount table, each flagged with whether a
    /// mount sits at *exactly* that path.
    ///
    /// The flag is what decides who wins a name collision with the in-memory tree: an exact
    /// child is a mount point and shadows whatever the tree had there, while a name that
    /// merely leads to a deeper mount keeps the tree's entry.
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

    /// Whether any mount lies strictly below `prefix` — i.e. whether `prefix` is a directory
    /// on the way to one.
    fn spans_mounts(&self, prefix: &Path) -> bool {
        self.descendant_mounts(prefix).next().is_some()
    }

    /// Whether `key` belongs to the mount table rather than to a store — a mount point itself,
    /// or a directory on the way to one.
    ///
    /// Neither is the filesystem's to move. A mount point is a real path its own store serves,
    /// but not one a `rename` may relocate, because doing so would rewrite the mount table
    /// through a file operation.
    fn is_mount_table_owned(&self, key: &Path) -> bool {
        self.mounts.contains_key(key) || self.spans_mounts(key)
    }

    /// Refuse a mutation aimed at a directory on the way to a mount.
    ///
    /// Checked *before* the store is consulted, because the in-memory tree holds only the
    /// directory, not what is mounted below it — asked first it would let `rmdir` take away
    /// a directory the mount table still needs, which is how a mount ends up detached.
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
/// Only *mount* paths carry this restriction, not request paths: a request may legitimately
/// name a non-UTF-8 file inside a passthrough store. But a mount point's own name gets
/// *listed* — reported through [`Dirent::name`], which is a `String` — so a non-UTF-8 component
/// could only be shown lossily, and a lossy name does not round-trip. That would produce an
/// entry visible in a listing whose `lookup` then fails.
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

            // Split the store's entries by whether the mount table also names them, so each
            // name is emitted exactly once.
            let (mut named_by_mounts, store_only): (Vec<_>, Vec<_>) = entries
                .into_iter()
                .partition(|entry| children.contains_key(&entry.name));
            let mut claimed: BTreeMap<String, Dirent> = named_by_mounts
                .drain(..)
                .map(|entry| (entry.name.clone(), entry))
                .collect();

            // Mount-derived names go first, in sorted order, so their positions depend only on
            // the mount table. `readdir`'s cursor is a position in this list and the kernel
            // resumes by quoting one, so putting them last would let a single file appearing in
            // the store shift every mount point — dropping a whole mount out of an in-progress
            // listing.
            let mut out = Vec::with_capacity(children.len() + store_only.len());
            for (name, mounted_here) in children {
                let store_entry = claimed.remove(&name);
                out.push(match (mounted_here, store_entry) {
                    // A mount at this exact path shadows whatever the tree had, the way any
                    // mount hides the directory it is mounted over. No stat: the kernel will
                    // `lookup` for metadata, and that lookup routes to the mount, so the two
                    // answers cannot disagree.
                    (true, _) => Dirent::new(name, DirentKind::Dir),
                    // On the way to a deeper mount — the tree's own directory, which carries
                    // metadata a made-up entry would lack.
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
            // The name is taken by a directory the mount table needs, which is what
            // `AlreadyExists` says — and what an exclusive create owes its caller.
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
            // Not empty: it holds the mount points below it, and those are not the caller's to
            // remove through the filesystem.
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

            // Neither end may be something the mount table owns. Checked before the stores are
            // consulted, because a refusal has to leave the tree untouched and a store asked
            // about its own root would answer something arbitrary.
            if self.is_mount_table_owned(&from_key) || self.is_mount_table_owned(&to_key) {
                return Err(io::ErrorKind::ReadOnlyFilesystem.into());
            }

            // Two stores cannot hand an object to each other, so this is `EXDEV` — and `mv`
            // reads that as "copy, then delete", which is the right recovery across a store
            // boundary. Doing the copy here instead could not be made atomic on partial
            // failure, and `mv` already knows how to deal with that.
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

        // The directory on the way to the mount is the table's, not the caller's to remove.
        assert_eq!(
            dir.rmdir(Path::new("deep")).await.unwrap_err().kind(),
            io::ErrorKind::DirectoryNotEmpty
        );
        // A move between the two is a move between stores.
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
