//! `index` — full-text search over the tree a session is working in, kept in an index of its
//! own.
//!
//! ```text
//! index ingest notes /docs          # makes the store if it is not there
//! index search notes -n 5 borrow checker
//! index sync   notes /docs          # the index says what the tree says
//! index purge  notes /docs/old      # ingest undone, and it needs no mount
//! index list
//! index drop   notes
//! ```
//!
//! # One name, two subcommands
//!
//! `ingest` and `search` are subcommands rather than two registered names because they are one
//! thing to whoever runs them: they share an index, and half of the pair is useless on its own.
//! A server exposes [`ExecutableSet::names`](cortex::exec::ExecutableSet::names), so two names
//! would be two things a client could be given separately — one name is what makes the pair
//! indivisible.
//!
//! ```no_run
//! # fn main() -> std::io::Result<()> {
//! use cortex::exec::ExecutableSet;
//! use cortex_exec_index::Index;
//!
//! // One root, on the host. Which stores live under it is the session's to decide.
//! let index = Index::new("/var/lib/agent/indexes")?;
//! let execs = ExecutableSet::new().register("index", Index::SUMMARY, index);
//! # Ok(())
//! # }
//! ```
//!
//! # The pieces
//!
//! [`Index`] is the only one a consumer names: the command, and the root every store lives
//! under. It parses a line, resolves the paths it was given through the mount the call was
//! handed, and answers in lines. Behind it are one `Store` per index — schema, the single
//! writer, the reader searches are answered from — and the `Hit` a search renders, neither of
//! which anyone outside has to hold.
//!
//! # Keeping up with a tree that moves
//!
//! `ingest` is idempotent for everything except a file that is *gone*: run it again and it
//! picks up what was added and replaces what changed, but a document stays behind for what
//! was removed. `sync` is that one case — the same walk, plus the documents the walk did not
//! produce, taken out.
//!
//! Which is why `ingest` is not simply `sync`: one only ever adds, the other deletes, and an
//! agent reaching for the first should not have to know the second's rules. `sync` also pays
//! for a pass over the store's documents, which is not a price a single file's ingest should
//! carry.
//!
//! There is deliberately no glob here. A shell in the guest expands one before this is even
//! called, so patterns already work where there is a shell — and a pattern could not close the
//! gap above in any case, because a file that is gone matches nothing.
//!
//! # A store is named, and the name is not a path
//!
//! Every subcommand names the index it works on, so a session can keep more than one and say
//! which it means — and `ingest` makes the one it names, so a caller never has to arrange a
//! store before using it. Which is the shape `memstore` has, for the same reason.
//!
//! Where they differ is *where* the thing lives, and they differ because the two are
//! different kinds of thing. A `memstore` store is a **document**: one file of the tree, which the
//! agent may read, copy or commit. An index is **derived state** — nobody reads it directly,
//! and deleting it costs a re-ingest — so it lives under a root on the host that the caller
//! was given rather than in the tree. `<STORE>` is one path component, so every index this
//! can reach is one somebody meant it to have.
//!
//! A name cannot be found by looking, the way a path could be — so `list` reports what there
//! is, and each subcommand's `--help` carries the same list under its usage.
//!
//! # Why this crate needs a mount, and only for half of what it does
//!
//! `ingest` reads files. The names it is given — `/notes` — are names in the *workspace*, and
//! only a [`Mount`](cortex::fs::Mount) turns one into a file this process can open, which is
//! why [`Executable::exec`](cortex::exec::Executable::exec) is handed one and why `ingest`
//! refuses when there is none.
//!
//! `search` and `purge` do not. The bytes were read when they were ingested, so a query is
//! answered from the index alone. `purge` is the sharper case: a file gone from the tree is
//! the reason to run it, and requiring a mount would make the one thing it exists for the one
//! thing it could not do.
//!
//! # The index is not in the tree
//!
//! [`Index::new`] takes a host path, and nothing writes into the mount. tantivy addresses its
//! segments with `mmap` and holds a lock file for the writer, neither of which a FUSE mount
//! owes anyone — under FUSE-T the mount is an NFS one, where both are exactly the operations
//! that behave differently. The tree is the *corpus*; the index is this process's own state,
//! and keeping them apart is also what lets the index survive a session whose mount is gone.
//!
//! That the root belongs outside a mount is a rule for whoever constructs this and **not a
//! check here**: nothing in the process can tell a [`Mount`](cortex::fs::Mount) from a plain
//! directory, and the standalone binary is the case that proves it — its "mount" is the
//! working directory and its root sits inside it, which is right there and would be wrong
//! under FUSE.
//!
//! # Paths are workspace paths
//!
//! A document's id is the path *inside the workspace*, so a result names a file the command
//! that asked for it can open. Resolving a relative argument needs
//! [`ExecCall::cwd`](cortex::exec::ExecCall::cwd); where a backend reports none, a relative
//! argument is refused and `/notes/a.md` is the spelling that works.

mod exec;
mod ingest;
mod search;
mod store;

// [`Index`] is the whole of it. A consumer names a root and registers the executable; what a
// store is, and what one answer looks like, are this crate's own business — and were public
// only while a caller had to build a `Store` to construct an `Index`, which is no longer how
// one is made. A structured result would be the reason to export `Hit`, and there is no API
// that hands one back yet.
pub use exec::Index;
