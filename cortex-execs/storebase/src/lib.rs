//! The store format `mem` and `index` share, and the SQLite handling that comes with it.
//!
//! The two are different commands over one file format, and the parts of them that differ are
//! meant to stay that way. What they are not is two answers to the questions both have to answer
//! anyway: what the tables are, how a store is opened, what it says when the file is somebody
//! else's, and what a `rusqlite` failure reads as. Identical by accident is the state that
//! drifts, so those live here once.
//!
//! [`sqlite::SCHEMA`] is where the tables are written down. What separates a store of one kind
//! from a store of the other is `meta.kind`, which [`sqlite::create`] writes and [`sqlite::open`]
//! insists on. Structure cannot say it: the structure is what they share.
//!
//! # The rule for what may come here
//!
//! **What has to be identical for a store file to be one thing lives here. What merely looks
//! alike today stays apart.**
//!
//! The first half is why the schema, `meta` and the write are here: two commands producing
//! files that a tool is supposed to read the same way cannot each own their own answer to what
//! a row is, or the two drift and the format stops being one format.
//!
//! The second half is why `search` is not. A shared statement would have to return the union
//! of two questions — the path and a snippet for one
//! caller, the whole text for the other — and each would throw away the other's half. Unlike an
//! unused *column*, that is work per row: `snippet()` runs whether or not anybody reads it, and
//! over 5,000 rows the wide statement cost 1.2× at a limit of 10 and 1.7× at 200. The row
//! mapping differs in each crate either way.
//!
//! Command surfaces and domain types are not here and would not be even if they matched today:
//! a `Memory` is not a document, and `mem insert` is not `index ingest`.

pub mod sqlite;
