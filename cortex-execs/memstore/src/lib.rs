//! `memstore` — a store that is one file in this tree, written by whoever calls it.
//!
//! ```text
//! memstore init   notes.sqlite
//! memstore insert notes.sqlite "User switched to oat milk" "User drinks tea"
//! memstore search notes.sqlite "oat milk"
//! ```
//!
//! # What this is, next to `mem`
//!
//! `insert` is what separates them. `mem` is handed a *conversation* and asks a model what in
//! it is worth keeping; this is handed the text to keep. So there is no provider here, no key
//! to read out of the environment, and no call that can cost money or fail on a network — an
//! `insert` either writes what it was given or says why it could not. `init` and `search` are
//! the same commands.
//!
//! The stores are **not** the same file any more, and the version says so: `mem` is schema `1`,
//! a contentless index over terms a segmenter cut; this is schema `2`, which hands the text to
//! FTS5 as written and lets `porter` cut it. Either build opening the other's store is told
//! which version it found rather than left to fail on a missing column. This crate's `store`
//! module says why the shape changed and what it keeps open.
//!
//! That makes the two useful for different callers rather than one being a lesser version of
//! the other. A session that has just finished talking wants `mem`, which decides. Something
//! that already knows what it wants remembered — a tool writing down a result, a person, an
//! agent that has done its own deciding — wants this, which does not.

mod exec;
mod memory;
mod store;

pub use exec::MemStore;
