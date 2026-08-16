//! What a [`WorkFs`] gains from this module: skills mounted into the tree, and a registry of
//! where they are.
//!
//! The methods live here rather than beside the mount table because the dependency runs one
//! way. `fs` knows nothing about skills — it holds a set of paths and prunes it when a mount
//! goes away, which is bookkeeping about its own tree — and this module is what gives that set
//! a meaning. A `WorkFs` compiled without ever naming a skill is the same `WorkFs`.

use std::{
    io,
    path::{Path, PathBuf},
};

use crate::fs::{FileSystem, WorkFs};
use crate::skill::{Skill, SkillDir};

/// A skill found in a workspace: where it is mounted, and what its manifest says.
///
/// The two are separate answers to separate questions and neither implies the other. `path` is
/// where the files are, which is what a command must be told to read them; `manifest` is what
/// the skill calls *itself*, which is what a caller announces — and a skill mounted at
/// `tools/summarize` may perfectly well be named `summarize`.
///
/// `manifest` is a [`Result`] because one unreadable skill must not take the listing down with
/// it. A workspace holding six good skills and one whose `skill.md` was hand-edited into
/// invalid frontmatter still has six usable skills, and the seventh is a misconfiguration
/// somebody has to see — which a listing that dropped it silently would hide.
pub struct SkillEntry {
    pub path: PathBuf,
    pub manifest: io::Result<Skill>,
}

impl WorkFs {
    /// Mount a skill directory at `path` and record it as a skill.
    ///
    /// [`mount`](WorkFs::mount)'s refusals, plus the mark: the path may not escape the
    /// workspace root, and a store already mounted there is [`AlreadyExists`]. A failure
    /// records nothing, so a workspace never claims a skill it did not mount.
    ///
    /// [`AlreadyExists`]: io::ErrorKind::AlreadyExists
    pub fn mount_skill<T: FileSystem + 'static>(
        &mut self,
        path: impl AsRef<Path>,
        skill: SkillDir<T>,
    ) -> io::Result<()> {
        let key = self.key(path.as_ref())?;
        self.mount(&key, skill)?;
        self.skills.insert(key);
        Ok(())
    }

    /// Builder-style [`mount_skill`](Self::mount_skill), overwriting any store already at
    /// `path` — the same trade [`try_with_mount`](WorkFs::try_with_mount) makes, for the same
    /// reason: a builder chain has nowhere to put a recovery, and a workspace being assembled
    /// has no earlier state worth protecting.
    pub fn try_with_skill<T: FileSystem + 'static>(
        self,
        path: impl AsRef<Path>,
        skill: SkillDir<T>,
    ) -> io::Result<Self> {
        let key = self.key(path.as_ref())?;
        let mut workfs = self.try_with_mount(&key, skill)?;
        workfs.skills.insert(key);
        Ok(workfs)
    }

    /// Write `skill`'s files into the tree at `path`, then record it as a skill.
    ///
    /// The other direction from [`mount_skill`](Self::mount_skill): instead of grafting a store
    /// that *is* the skill, this puts the skill's files into a store the workspace already has
    /// — a scratch directory, a project checkout — so the skill lands where the rest of that
    /// tree lives and is written back to whatever backs it.
    ///
    /// Whatever serves `path` must be writable, and must be something: a path no mount claims
    /// is refused before any bytes are written, since a skill assembled into nothing is a mark
    /// pointing at an empty tree. See [`Skill::write_to`] for what a failure partway leaves
    /// behind — the mark is not made in that case, so the workspace does not claim a skill it
    /// only half wrote.
    pub async fn write_skill(&mut self, path: impl AsRef<Path>, skill: &Skill) -> io::Result<()> {
        let key = self.key(path.as_ref())?;
        if !self.is_claimed(&key) {
            return Err(io::ErrorKind::NotFound.into());
        }
        skill.write_to(&*self, &key).await?;
        self.skills.insert(key);
        Ok(())
    }

    /// Record that the tree already at `path` is a skill.
    ///
    /// For a directory that arrived some other way — inside a passthrough of a project that
    /// keeps its skills in a subdirectory, or written by a command the agent itself ran — and
    /// so was never mounted as one.
    ///
    /// **The mark is a claim, not a verdict.** All that is checked is that some store serves
    /// the path, because that much is knowable from the mount table alone and marking a path
    /// nothing serves is always a mistake. Whether a readable `skill.md` is actually there is a
    /// question for the store, and it is asked — and answered per skill — by
    /// [`skill_entries`](Self::skill_entries).
    pub fn mark_as_skill(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let key = self.key(path.as_ref())?;
        if !self.is_claimed(&key) {
            return Err(io::ErrorKind::NotFound.into());
        }
        self.skills.insert(key);
        Ok(())
    }

    /// Drop the mark at `path`, leaving the files where they are. `true` if it was marked.
    ///
    /// Unmarking is not unmounting, and this is the whole difference: the tree stays exactly
    /// as readable as it was, and only stops being announced as a skill. Taking the files away
    /// is [`unmount`](WorkFs::unmount), which drops the mark too.
    pub fn unmark_skill(&mut self, path: impl AsRef<Path>) -> io::Result<bool> {
        let key = self.key(path.as_ref())?;
        Ok(self.skills.remove(&key))
    }

    /// Whether `path` is marked as a skill. A path that cannot be a mount key — one that
    /// escapes the root — is not one, rather than an error.
    pub fn is_skill(&self, path: impl AsRef<Path>) -> bool {
        self.key(path.as_ref())
            .is_ok_and(|key| self.skills.contains(&key))
    }

    /// Every skill in this workspace, by path, in sorted order.
    ///
    /// The cheap listing: the registry as it stands, with nothing read. A caller that needs to
    /// know what the skills *are* wants [`skill_entries`](Self::skill_entries), which pays for
    /// a manifest each.
    pub fn skills(&self) -> impl Iterator<Item = &Path> {
        self.skills.iter().map(|path| path.as_path())
    }

    /// Every skill in this workspace with its manifest read — the listing to hand something
    /// that has to *choose* a skill, since choosing needs the descriptions.
    ///
    /// One read per skill, so this is a call to make when the set of skills changes rather than
    /// per request; nothing here is cached, because a store's files may change under the
    /// workspace at any moment and a stale description is worse than a re-read.
    ///
    /// Sequential rather than concurrent: the registry is a handful of entries whose stores are
    /// usually memory or a local directory, and joining a set of futures would buy microseconds
    /// at the cost of the tree being read from several places at once.
    pub async fn skill_entries(&self) -> Vec<SkillEntry> {
        let paths: Vec<_> = self.skills.iter().cloned().collect();
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            let manifest = Skill::read(self, &path).await;
            out.push(SkillEntry { path, manifest });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{InMemFs, PassthroughFs};

    async fn skill_dir(name: &str) -> SkillDir<InMemFs> {
        SkillDir::from_inmem(&Skill::new(name, format!("what {name} does")))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_mounted_skill_is_listed_and_readable_through_the_workspace() {
        let mut workfs = WorkFs::new();
        workfs
            .mount_skill("tools/summarize", skill_dir("summarize").await)
            .unwrap();

        assert_eq!(
            workfs.skills().collect::<Vec<_>>(),
            [Path::new("tools/summarize")]
        );
        let entries = workfs.skill_entries().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].manifest.as_ref().unwrap().name, "summarize");
        // And the same files are there under the mount point, for anything reading the tree.
        assert!(
            workfs
                .stat(Path::new("tools/summarize/skill.md"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_skill_mounted_where_a_store_already_is_is_refused_and_not_recorded() {
        let mut workfs = WorkFs::new();
        workfs.mount("tools", InMemFs::new()).unwrap();
        assert_eq!(
            workfs
                .mount_skill("tools", skill_dir("summarize").await)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(workfs.skills().count(), 0);
    }

    #[tokio::test]
    async fn a_skill_written_into_an_existing_store_is_marked_and_readable() {
        let mut workfs = WorkFs::new();
        workfs.mount("", InMemFs::new()).unwrap();
        let skill = Skill::new("summarize", "Claims only.")
            .try_with_file("refs/style.md", "terse")
            .unwrap();

        workfs.write_skill("work/skills/sum", &skill).await.unwrap();

        assert!(workfs.is_skill("work/skills/sum"));
        let entries = workfs.skill_entries().await;
        assert_eq!(
            entries[0].manifest.as_ref().unwrap().description,
            "Claims only."
        );
        assert!(
            workfs
                .stat(Path::new("work/skills/sum/refs/style.md"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn writing_a_skill_where_nothing_is_mounted_is_refused() {
        let mut workfs = WorkFs::new();
        assert_eq!(
            workfs
                .write_skill("nowhere", &Skill::new("n", "d"))
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(workfs.skills().count(), 0);
    }

    /// A skill directory that came in as part of a bigger tree, named as one afterwards.
    #[tokio::test]
    async fn a_subtree_can_be_marked_after_the_fact() {
        let mut workfs = WorkFs::new();
        workfs.mount("project", InMemFs::new()).unwrap();
        Skill::new("review", "Review a diff.")
            .write_to(&workfs, Path::new("project/.skills/review"))
            .await
            .unwrap();

        workfs.mark_as_skill("project/.skills/review").unwrap();
        assert!(workfs.is_skill("project/.skills/review"));
        assert_eq!(
            workfs.skill_entries().await[0]
                .manifest
                .as_ref()
                .unwrap()
                .name,
            "review"
        );

        assert!(workfs.unmark_skill("project/.skills/review").unwrap());
        assert_eq!(workfs.skills().count(), 0);
        // Unmarking left the files alone.
        assert!(
            workfs
                .stat(Path::new("project/.skills/review/skill.md"))
                .await
                .is_ok()
        );
    }

    #[test]
    fn marking_a_path_no_store_serves_is_refused() {
        let mut workfs = WorkFs::new();
        assert_eq!(
            workfs.mark_as_skill("nowhere").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(workfs.mark_as_skill("../escape").is_err());
    }

    /// The mark says a skill is there; the read says whether it is. A directory with no
    /// manifest is reported as the broken skill it is, and does not hide the working one.
    #[tokio::test]
    async fn an_unfounded_mark_surfaces_per_skill_in_the_listing() {
        let mut workfs = WorkFs::new();
        workfs.mount("", InMemFs::new()).unwrap();
        workfs.mkdir(Path::new("empty")).await.unwrap();
        workfs.mark_as_skill("empty").unwrap();
        workfs
            .write_skill("real", &Skill::new("real", "works"))
            .await
            .unwrap();

        let entries = workfs.skill_entries().await;
        let broken = entries
            .iter()
            .find(|e| e.path == Path::new("empty"))
            .unwrap();
        assert_eq!(
            broken.manifest.as_ref().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let good = entries
            .iter()
            .find(|e| e.path == Path::new("real"))
            .unwrap();
        assert_eq!(good.manifest.as_ref().unwrap().name, "real");
    }

    /// Unmounting the store that served a skill takes the mark with it — the alternative is a
    /// registry that announces a tree nothing serves.
    #[tokio::test]
    async fn unmounting_drops_the_marks_it_orphans() {
        let mut workfs = WorkFs::new();
        workfs
            .mount_skill("tools/summarize", skill_dir("summarize").await)
            .unwrap();
        workfs.unmount("tools/summarize").unwrap();
        assert_eq!(workfs.skills().count(), 0);
    }

    /// But only the marks it orphans: a skill under a deeper mount of its own is still served
    /// after the mount above it goes.
    #[tokio::test]
    async fn unmounting_keeps_a_mark_another_mount_still_serves() {
        let mut workfs = WorkFs::new();
        workfs.mount("", InMemFs::new()).unwrap();
        workfs
            .mount_skill("tools/summarize", skill_dir("summarize").await)
            .unwrap();

        workfs.unmount("").unwrap();
        assert_eq!(
            workfs.skills().collect::<Vec<_>>(),
            [Path::new("tools/summarize")]
        );
    }

    /// A host directory served where it lies, named as a skill on the way in.
    #[test]
    fn a_host_skill_directory_mounts_as_a_skill() {
        let mut workfs = WorkFs::new();
        workfs
            .mount_skill(
                "tools/local",
                SkillDir::from_passthrough("/does/not/matter"),
            )
            .unwrap();
        assert!(workfs.is_skill("tools/local"));
        // The same store, unwrapped, is just a store — the claim is the wrapper's.
        assert_eq!(
            SkillDir::from_passthrough("/x").into_store().root(),
            PassthroughFs::new("/x").root()
        );
    }
}
