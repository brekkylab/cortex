//! `memstore` — a store that is one file in this tree, written by whoever calls it.
//!
//! ```text
//! memstore init   notes.sqlite
//! memstore insert notes.sqlite "User switched to oat milk" "User drinks tea"
//! memstore search notes.sqlite "oat milk"
//! ```
//!
//! # What it does not do
//!
//! It does not decide. What is handed to `insert` is what the store holds, word for word — no
//! model reads it, nothing shortens or rephrases it, and there is no provider or key anywhere in
//! this crate. An `insert` either writes what it was given or says why it could not; it cannot
//! cost money and it cannot fail on a network.
//!
//! That is what makes it usable by something that has already done its own deciding — a tool
//! writing down a result, an agent recording a conclusion, a person. Anything that wants a
//! judgement made about a conversation first has to make it before calling.
//!
//! # What it shares with `docstore`
//!
//! The file. Same tables, same `meta`, same write — see [`cortex_exec_storebase::sqlite`] — and
//! `meta.kind` is what tells one from the other, since the structure is what they share. What differs is
//! which columns each fills: a memory has an `id` and no `path`, so nothing collides and the
//! same sentence can be stored twice; a document has a `path`, which is its identity and what
//! makes `docstore ingest` idempotent.

mod exec;
mod memory;
mod store;

pub use exec::MemStore;
