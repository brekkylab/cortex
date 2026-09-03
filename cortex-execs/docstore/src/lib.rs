//! `docstore` — full-text search over the tree a session is working in, kept in a store that
//! is one file of that tree.
//!
//! ```text
//! docstore init   notes.db
//! docstore ingest notes.db /docs          # a directory means everything under it
//! docstore search notes.db -n 5 borrow checker
//! docstore sync   notes.db /docs          # the store says what the tree says
//! docstore purge  notes.db /docs/old      # ingest undone
//! docstore list                           # the stores beside this one
//! docstore drop   notes.db
//! ```
//!
//! ```no_run
//! # fn main() {
//! use cortex::exec::ExecutableSet;
//! use cortex_exec_docstore::DocStore;
//!
//! let execs = ExecutableSet::new().register("docstore", DocStore::SUMMARY, DocStore::new());
//! # }
//! ```
//!
//! # What this is, next to the other two
//!
//! `memstore` takes text a caller hands it; this takes files, and remembers where each one came
//! from. That is the whole difference between them as commands — and it is why the answer to a
//! search here names a path the caller can open, rather than only the text.
//!
//! `cortex-exec-index` does the same job on tantivy. This does it on SQLite's FTS5, which is
//! the engine `memstore` already uses, so a workspace that registers both links one search
//! engine rather than two. The two are meant to be interchangeable from outside:
//! same commands, same output shapes, same walk. Where they differ is written down in
//! [`DocStore`] and in this crate's `store` module — the store is a file in the tree rather
//! named directory on the host, and everything that follows from that.
//!
//! # Vectors: the schema is ready, and there is nothing in it
//!
//! A store carries a `chunk` table with an `embedding` column, and `meta` has room for the
//! model and the dimension that produced them. **All of it is empty.** There is no embedder in
//! this workspace to fill it — ailoy, the only language-model runtime here, has no embedding
//! API — and choosing a chunk size before there is a model to size chunks for would be
//! deciding on no evidence.
//!
//! Since a document's body is stored whole, chunks stay derivable at any later time: the day an
//! embedder arrives, the work is a backfill rather than a re-ingest. Recording the model in
//! `meta` is what makes that backfill able to notice that the vectors it finds were written by
//! a different model, which is the difference between re-embedding and ranking wrongly in
//! silence.

mod exec;
mod ingest;
mod search;
mod store;

pub use exec::DocStore;
