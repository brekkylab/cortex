//! [`SkillDir`] — a store whose root is a skill directory.

use std::{
    io,
    path::{Path, PathBuf},
};

use crate::BoxFuture;
use crate::fs::{Dirent, FileSystem, InMemFs, PassthroughFs, Stat};
use crate::skill::Skill;

/// A [`FileSystem`] whose root *is* a skill: `skill.md` at the top, and whatever the
/// instructions bring with them beside it.
///
/// The type is the claim. A [`WorkFs`](crate::fs::WorkFs) grafts stores at paths and has no way
/// to tell one tree from another, so `mount_skill(path, dir)` taking a `SkillDir` rather than a
/// `FileSystem` is what separates "a directory that happens to hold a manifest" from "a skill,
/// mounted here". Nothing else about the store changes: it is still a store, and
/// [`mark_as_skill`](crate::fs::WorkFs::mark_as_skill) exists for the tree that was already
/// mounted before anyone called it a skill.
///
/// Generic over the store rather than boxed, because the three ways a skill directory comes
/// about are three different stores and the caller usually still wants theirs: built in memory
/// from a [`Skill`], read from a directory on this host, or handed in already assembled — an
/// object store, another `WorkFs`, a store this crate has never heard of.
///
/// **Not validated at construction.** Making one does not read the manifest, and cannot:
/// [`FileSystem`] is async and these constructors are the cheap, infallible half of the API —
/// `from_passthrough` in particular names a directory that need not exist yet. What a caller
/// gets instead is [`manifest`](Self::manifest), which reads it and says precisely what is
/// wrong.
pub struct SkillDir<T: FileSystem> {
    store: T,
}

impl<T: FileSystem> SkillDir<T> {
    /// Take an existing store to be a skill directory, as it stands.
    ///
    /// The general constructor, and the one the other two are written in terms of: whatever
    /// already serves the skill's files — an object store, a `WorkFs` assembled from several
    /// stores, a backend of the caller's own — becomes a skill by being named one here.
    pub fn new(store: T) -> Self {
        SkillDir { store }
    }

    pub fn store(&self) -> &T {
        &self.store
    }

    /// Give the store back, dropping the claim that it is a skill.
    pub fn into_store(self) -> T {
        self.store
    }

    /// Read the manifest at the root of this directory.
    ///
    /// This is where an unfounded claim surfaces: a store with no `skill.md` in it answers
    /// [`NotFound`](io::ErrorKind::NotFound) here, and one whose manifest cannot be parsed
    /// [`InvalidData`](io::ErrorKind::InvalidData).
    pub async fn manifest(&self) -> io::Result<Skill> {
        Skill::read(&self.store, Path::new("")).await
    }
}

impl SkillDir<InMemFs> {
    /// Build the directory in memory from a [`Skill`] — the manifest written out, and every
    /// file the skill carries laid down beside it.
    ///
    /// The one constructor that is async, because it is the one that writes: a skill that
    /// exists only as a value in this process becomes a directory by being materialized into a
    /// store, and there is no cheaper moment for it than this.
    ///
    /// Nothing here touches a disk. What the skill says is what the tree holds, so a caller
    /// composing skills at run time — from a template, from a model's output, from a database —
    /// gets a mountable directory without a scratch directory to clean up.
    pub async fn from_inmem(skill: &Skill) -> io::Result<Self> {
        let store = InMemFs::new();
        skill.write_to(&store, Path::new("")).await?;
        Ok(SkillDir::new(store))
    }
}

impl SkillDir<PassthroughFs> {
    /// A skill directory that already exists on this host, served from where it is.
    ///
    /// Nothing is copied and nothing is read: the directory stays the source of truth, so a
    /// skill edited on disk is edited in the workspace too. `root` need not exist yet —
    /// [`PassthroughFs`] anchors without touching the filesystem, and a missing directory
    /// surfaces at the first operation.
    pub fn from_passthrough(root: impl Into<PathBuf>) -> Self {
        SkillDir::new(PassthroughFs::new(root))
    }
}

/// A skill directory is a directory: every call goes to the store inside, untouched.
///
/// So a `SkillDir` can be mounted, listed and read like any other tree — by
/// [`mount_skill`](crate::fs::WorkFs::mount_skill), which needs exactly that, and by a caller
/// who wants to read the skill's files without unwrapping it first. Each method hands the
/// inner future straight back, so the wrapper costs nothing per call.
impl<T: FileSystem> FileSystem for SkillDir<T> {
    fn stat<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        self.store.stat(path)
    }

    fn list<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        self.store.list(path)
    }

    fn read_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a mut [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        self.store.read_at(path, buf, offset)
    }

    fn create<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        self.store.create(path)
    }

    fn mkdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        self.store.mkdir(path)
    }

    fn unlink<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        self.store.unlink(path)
    }

    fn rmdir<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        self.store.rmdir(path)
    }

    fn write_at<'a>(
        &'a self,
        path: &'a Path,
        buf: &'a [u8],
        offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        self.store.write_at(path, buf, offset)
    }

    fn truncate<'a>(&'a self, path: &'a Path, size: u64) -> BoxFuture<'a, io::Result<()>> {
        self.store.truncate(path, size)
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        self.store.rename(from, to)
    }

    fn flush<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, io::Result<()>> {
        self.store.flush(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_skill_built_in_memory_reports_its_own_manifest() {
        let dir = SkillDir::from_inmem(&Skill::new("summarize", "Claims only."))
            .await
            .unwrap();
        assert_eq!(dir.manifest().await.unwrap().description, "Claims only.");
    }

    /// The claim is not checked when it is made, so an empty store is a `SkillDir` right up
    /// until somebody reads it.
    #[tokio::test]
    async fn a_store_without_a_manifest_fails_at_the_read_and_not_before() {
        let dir = SkillDir::new(InMemFs::new());
        assert_eq!(
            dir.manifest().await.unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[tokio::test]
    async fn a_skill_directory_is_readable_as_a_filesystem() {
        let skill = Skill::new("s", "d")
            .try_with_file("refs/notes.md", "hello")
            .unwrap();
        let dir = SkillDir::from_inmem(&skill).await.unwrap();

        let mut buf = [0u8; 5];
        let n = dir
            .read_at(Path::new("refs/notes.md"), &mut buf, 0)
            .await
            .unwrap();
        assert_eq!(&buf[..n], b"hello");
    }
}
