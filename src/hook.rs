//! Change hook for a [`Workspace`](crate::Workspace)'s mutations.
//!
//! Lets a host react to writes — ingestion/indexing, audit, cache invalidation —
//! without cortex knowing what the reaction is. Attach with
//! [`Workspace::with_hook`](crate::Workspace::with_hook); an unset hook fires
//! nothing. Any classification ("is this under `knowledge/`?") and identity
//! ("which workspace") live in the impl, not here.
//!
//! A write fires exactly once, on `flush`, with the create-vs-modify distinction
//! decided by whether the path existed at open — so a hook that re-indexes sees
//! the finished bytes, not a half-written file.

/// A mutation to a workspace. Paths are workspace-relative (no leading slash,
/// e.g. `files/knowledge/a.txt`) — the same path the caller addressed.
pub enum FsEvent<'a> {
    /// A new file appeared at a previously-absent path.
    Created(&'a str),
    /// An existing file was overwritten in place.
    Modified(&'a str),
    /// A file or directory was removed.
    Removed(&'a str),
}

/// Observes mutations to a [`Workspace`](crate::Workspace). The workspace calls
/// this after the change lands.
pub trait FsHook: Send + Sync {
    fn on_change(&self, event: FsEvent<'_>);
}
