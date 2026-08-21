//! Skills: instructions an agent can find by looking, rather than by being told.
//!
//! A skill is a directory. `skill.md` at its root says what the skill is called, what it is for
//! and how to carry it out; whatever else the directory holds — scripts, templates, references
//! — is the material those instructions point at. That is the whole format, and it is a format
//! on purpose: the agent reads a skill with `cat` and `ls`, the two things it already knows,
//! and a capability added to a workspace arrives as files to read rather than as an interface
//! to learn. It is the same bargain [`fs`](crate::fs) makes, one level up.
//!
//! Three types, one for each of those sentences:
//!
//! * [`Skill`] — the definition, as a value: a name, a description, instructions, and the files
//!   that come with them. What a caller builds, and what a manifest parses back into.
//! * [`SkillDir`] — a [`FileSystem`](crate::fs::FileSystem) whose root is a skill directory.
//!   Built in memory from a [`Skill`], pointed at a directory on this host, or wrapped around a
//!   store the caller already has.
//! * [`SkillEntry`] — one skill found in a workspace: where it is, and what its manifest says.
//!
//! ```no_run
//! # use cortex::fs::WorkFs;
//! # use cortex::skill::{Skill, SkillDir};
//! # async fn example() -> std::io::Result<()> {
//! let summarize = Skill::new("summarize", "Condense a document into its claims.")
//!     .with_instructions("Read the file, then write one bullet per claim.");
//!
//! let mut workfs = WorkFs::new();
//! // A skill assembled in this process, mounted as a directory the agent can read.
//! workfs.mount_skill("skills/summarize", SkillDir::from_inmem(&summarize).await?)?;
//! // One that already exists on this host, served from where it lies.
//! workfs.mount_skill("skills/review", SkillDir::from_passthrough("/opt/skills/review"))?;
//!
//! for entry in workfs.skill_entries().await {
//!     if let Ok(skill) = entry.manifest {
//!         println!("{} at {}: {}", skill.name, entry.path.display(), skill.description);
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # A workspace keeps a registry, and the registry is marks
//!
//! A [`WorkFs`](crate::fs::WorkFs) cannot tell a skill directory from any other directory by
//! looking — every store is a tree, and finding the skills by walking the whole workspace for
//! `skill.md` would be a scan of an object store on every listing, over a question the caller
//! already knew the answer to when the tree was assembled. So a workspace holds the set of
//! paths somebody *said* were skills:
//! [`mount_skill`](crate::fs::WorkFs::mount_skill) grafts a [`SkillDir`] and records it,
//! [`write_skill`](crate::fs::WorkFs::write_skill) materializes a [`Skill`] into a store that is
//! already there and records that, and [`mark_as_skill`](crate::fs::WorkFs::mark_as_skill)
//! records a tree that arrived by some other route entirely.
//!
//! The registry is therefore a claim about the tree and not a fact derived from it, and the two
//! come apart the moment anything writes: a `skill.md` the agent deletes leaves a mark behind.
//! That is why the listing has two halves. [`skills`](crate::fs::WorkFs::skills) is the marks,
//! and costs nothing; [`skill_entries`](crate::fs::WorkFs::skill_entries) reads each manifest
//! and reports per skill, so a mark with nothing behind it shows up as the broken skill it is
//! instead of being quietly dropped — or quietly believed.

mod skill;
mod skill_dir;
mod workfs;

pub use skill::*;
pub use skill_dir::*;
pub use workfs::*;
