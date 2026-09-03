//! What the executables in `cortex-execs` had three copies of.
//!
//! `memstore` and `docstore` are two different commands over one store format, and the parts
//! of them that differ are meant to stay that way. What they are not is three different answers to the
//! questions every one of them has to answer anyway: how a store on a FUSE-mounted tree is
//! opened, what it says when the file is somebody else's, which stream clap's output goes to,
//! and what a workspace path resolves against.
//!
//! Those answers were identical — [`sqlite::sql_error`] was byte-for-byte the same function in
//! three files — and identical by accident is the state that drifts. So they live here once.
//!
//! # The store format is here too
//!
//! `memstore` and `docstore` keep the same tables, and [`sqlite::SCHEMA`] is where they are
//! written down once. What separates a store of one kind from a store of the other is
//! `meta.kind`, which [`sqlite::create`] writes and [`sqlite::open`] insists on. Structure
//! cannot say it: the structure is what they share.
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
//! `is_store` lives in `docstore` for the same reason: `list` and `drop` are its alone, so
//! `memstore` has no command that asks whether a file is a store. What is in [`sqlite`] is what
//! both call.
//!
//! Command surfaces and domain types are not here and would not be even if they matched today:
//! a `Memory` is not a document, and `memstore insert` is not `docstore ingest`.

pub mod command;
pub mod sqlite;
