//! What the window is looking at: one workspace, and the three lists that describe it.
//!
//! The tree itself is a [`WorkFs`] — the same type any other consumer of `cortex` assembles —
//! and every listing, read and write the window performs goes through it rather than through a
//! model of it kept on the JavaScript side. That is the whole reason this crate holds state at
//! all: a file tree drawn from a snapshot shipped to the webview would drift the moment a store
//! answers differently, and the stores here are Notion and S3, which answer differently often.
//!
//! **A session starts empty and lives in memory.** There is no load and no save yet, so the
//! workspace a launch produces is a fresh [`WorkFs`] with an [`InMemFs`] at its root; closing
//! the window is what discards it. The root store is what makes the tree writable at all — a
//! `WorkFs` with no mount claims no path, so a dropped file would have nowhere to land — and it
//! is the seam a later "open a saved workspace" replaces, since everything above addresses the
//! tree by path and never asks which store is underneath.
//!
//! The three lists beside it are what the tree cannot say about itself:
//!
//! * [`MountInfo`] — a mount table is queryable by path but does not report *what* is mounted:
//!   `WorkFs` holds `Box<dyn FileSystem>`, so "this directory is a Notion workspace" is knowable
//!   only to whoever mounted it. The sidebar draws from this.
//! * [`Agent`] — a registered agent: a system message and the resources it was given.
//!
//! Memories and docsets are deliberately *not* a fourth list. They live in the tree, under
//! `/.cortex`, as files like anything else — see [`ResourceKind`] — so they are carried by
//! whatever carries the workspace rather than by a `Vec` that a later save would have to learn
//! to serialize separately.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use cortex::fs::{InMemFs, WorkFs};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::{Error, Result};

/// Everything one window owns.
///
/// Three locks rather than one over a single struct, because they are contended by different
/// things: a listing takes `fs` for as long as a Notion page takes to render, and an agent form
/// saving while that is in flight has no reason to wait for it.
pub struct Workspace {
    /// The tree. `RwLock` and not `Mutex` because reads are what a file browser does: several
    /// listings and a read can overlap, and only mounting takes the tree exclusively.
    pub fs: RwLock<WorkFs>,
    pub mounts: RwLock<Vec<MountInfo>>,
    pub agents: RwLock<Vec<Agent>>,
    /// The source of every id handed to the window.
    ///
    /// A counter, not a UUID: these identify rows within one process that ends when the window
    /// closes, so there is nothing for a globally unique id to be unique against — and a
    /// readable `docset-3` is worth more in a screenshot of a bug report than 36 hex digits.
    next_id: AtomicU64,
}

impl Workspace {
    /// A new, empty workspace: a writable root and nothing in it.
    pub fn empty() -> Result<Self> {
        let fs = WorkFs::new().try_with_mount(ROOT_PATH, InMemFs::new())?;
        Ok(Workspace {
            fs: RwLock::new(fs),
            mounts: RwLock::new(vec![MountInfo {
                path: "/".into(),
                kind: MountKind::Scratch,
                label: "Workspace".into(),
                detail: "in-memory · 저장되지 않음".into(),
                writable: true,
            }]),
            agents: RwLock::new(Vec::new()),
            next_id: AtomicU64::new(1),
        })
    }

    /// `prefix-N`, unique for the life of the window.
    pub fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Record a mount, refusing a path another store already answers for.
    ///
    /// The check is here and not left to [`WorkFs::mount`] so that the two never disagree: the
    /// tree would accept a second mount at a path this list already names if the first were
    /// registered under a different spelling, and the sidebar would then show one entry for two
    /// stores.
    pub async fn remember_mount(&self, info: MountInfo) -> Result<()> {
        let mut mounts = self.mounts.write().await;
        if mounts.iter().any(|m| m.path == info.path) {
            return Err(Error::msg(format!(
                "{} 에는 이미 다른 저장소가 연결되어 있습니다",
                info.path
            )));
        }
        mounts.push(info);
        mounts.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(())
    }
}

/// The path a mount is registered at, as [`WorkFs`] spells the root: the empty path.
pub const ROOT_PATH: &str = "";

/// What kind of store sits behind a mount point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountKind {
    /// The in-memory root every session starts with.
    Scratch,
    /// A directory on this machine, served through `PassthroughFs`.
    Local,
    /// A Notion workspace, read-only.
    Notion,
    /// An S3 bucket, read-only.
    S3,
}

/// One row of the sidebar: a store, and what it is.
#[derive(Clone, Debug, Serialize)]
pub struct MountInfo {
    /// Where it hangs in the tree, `/`-rooted for display. The root store is `/`.
    pub path: String,
    pub kind: MountKind,
    /// The name shown for it.
    pub label: String,
    /// The one line under the name — a bucket, a host directory. **Never a credential:** an
    /// api key or a secret access key reaches this crate and stops in the store that took it.
    pub detail: String,
    pub writable: bool,
}

/// Whether a resource is a `mem` store or an `index` one.
///
/// **A resource lives in the workspace.** Registering one writes `<name>.json` under this kind's
/// directory in the tree, and that file is the resource — there is no list beside the tree
/// holding a copy. Two things follow, and both are the reason for it: whatever ends up carrying
/// a workspace (the save that does not exist yet, a `WorkFs` mounted for a guest) carries the
/// resources with it for free, and a docset written by something other than this window is
/// visible here the moment it appears.
///
/// The store itself is still to come. `mem` and `index` in `cortex-execs/` are the programs that
/// write one — a single SQLite file — and neither is wired up here, so what a registration
/// produces today is the manifest and a name for the store that will sit beside it. That is what
/// [`Resource::backed`] reports on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceKind {
    Memory,
    Docset,
}

impl ResourceKind {
    /// The directory in the workspace that holds this kind.
    ///
    /// Under a dot-directory because these are the workspace's own bookkeeping rather than
    /// anything a person put there, and a workspace whose root is a connected local folder
    /// should not grow two top-level directories nobody asked for.
    pub fn dir(self) -> &'static str {
        match self {
            ResourceKind::Memory => "/.cortex/memory",
            ResourceKind::Docset => "/.cortex/docset",
        }
    }
}

/// `name` as a single path component: lowercase, and nothing that could redirect a path.
///
/// The result is what a resource is filed under, so it decides identity — two names that slug
/// the same are the same resource, and the collision surfaces as the `AlreadyExists` that
/// creating the manifest answers with.
///
/// **A run of separators collapses to one.** Mapping each rejected character to its own `-`
/// would file `Team  Notes!` under `team--notes-` and `team notes` under `team-notes`, so two
/// names nobody would call different would quietly become two resources — and the second, being
/// a different file, would never hit the collision that is supposed to catch exactly this.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// A memory or a docset an agent can be given.
#[derive(Clone, Debug, Serialize)]
pub struct Resource {
    /// The manifest's path, which is also the resource's identity: it is where the resource is,
    /// and an agent holding it can be resolved by reading the tree rather than a registry.
    pub id: String,
    pub kind: ResourceKind,
    pub name: String,
    /// Free text the form collected — what this memory is for, which directory the docset will
    /// be ingested from.
    pub note: String,
    /// The `.sqlite` beside the manifest: where `mem` / `index` will write. `backed` says
    /// whether it is there yet.
    pub store_path: String,
    /// Whether [`Self::store_path`] exists in the workspace. `false` for everything this window
    /// creates today, and the label the UI shows is drawn from it rather than hard-coded, so the
    /// day a store does appear the window says so without changing.
    pub backed: bool,
    pub created_ms: u64,
}

/// What the agent form collects.
#[derive(Clone, Debug, Deserialize)]
pub struct AgentSpec {
    pub name: String,
    pub model: String,
    pub system_message: String,
    /// [`Resource::id`]s, as ticked in the form.
    #[serde(default)]
    pub resource_ids: Vec<String>,
    /// Workspace paths the agent should see, as picked from the tree.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// A registered agent.
///
/// Registered, not running. ailoy is what would run one, and nothing here starts it: an agent
/// is a record of a system message and what it was given to read, which is exactly the part
/// that has to exist before a runtime can be handed anything.
#[derive(Clone, Debug, Serialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub model: String,
    pub system_message: String,
    pub resource_ids: Vec<String>,
    pub paths: Vec<String>,
    pub created_ms: u64,
}

/// Milliseconds since the epoch, or `None` for a clock that predates it.
pub fn epoch_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Now, in milliseconds since the epoch.
pub fn now_ms() -> u64 {
    epoch_ms(SystemTime::now()).unwrap_or_default()
}
