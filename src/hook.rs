//! Change hook for a [`Workspace`](crate::Workspace)'s mutations.
//!
//! Lets a host react to writes — ingestion/indexing, audit, cache invalidation —
//! without cortex knowing what the reaction is. Attach with
//! [`Workspace::with_hook`](crate::Workspace::with_hook); an unset hook fires
//! nothing. Any classification ("is this under `knowledge/`?") and identity
//! ("which workspace") live in the impl, not here.
//!
//! A write fires exactly once, on the first *successful* `flush` or `commit`,
//! with the create-vs-modify distinction decided by whether the path existed at
//! open. A failed write-out fires nothing.
//!
//! "Finished bytes, not a half-written file" holds only when the frontend's
//! `flush` is its finalize — as it is for a WebDAV `PUT`, which flushes once at
//! the end. A frontend that flushes mid-write (a FUSE `fsync`, or a `close` on a
//! `dup`ed descriptor while writes still come through another) fires on that
//! first flush and then latches, so a hook attached to such a mount can observe
//! an intermediate state. Attach hooks to a flush-is-finalize frontend, or don't
//! rely on the tail of a mid-write mount.
//!
//! Events are path-level and not recursive: a directory `rename` fires
//! `Removed(old)` + `Created(new)` for the directory itself, not one event per
//! descendant, and `Created`/`Removed` may therefore name a directory. A
//! consumer that tracks file-level state should rescan the subtree on a
//! directory event.

/// A mutation to a workspace. Paths are workspace-relative (no leading slash,
/// e.g. `files/knowledge/a.txt`) — the same path the caller addressed.
pub enum FsEvent<'a> {
    /// Something appeared at a previously-absent path: a written file, or the
    /// destination of a `rename` (which may be a directory — see the module
    /// docs on non-recursive directory events).
    Created(&'a str),
    /// An existing file was overwritten in place.
    Modified(&'a str),
    /// A file or directory was removed (including the source of a `rename`).
    Removed(&'a str),
}

/// Observes mutations to a [`Workspace`](crate::Workspace). The workspace calls
/// this after the change lands.
pub trait FsHook: Send + Sync {
    fn on_change(&self, event: FsEvent<'_>);
}
