//! `mem` — a memory a session can write to and ask questions of, kept in one file of the tree
//! it is working in.
//!
//! ```text
//! mem insert notes.sqlite "the user prefers tea to coffee"
//! mem search notes.sqlite "what does the user drink"
//! ```
//!
//! # What it is modelled on
//!
//! mem0's shape: text goes in, *facts* come out of it, and each fact meets the facts already
//! held before anything is written — so a restatement replaces what it restates and a
//! contradiction retires it, rather than both sitting in the store waiting to confuse whoever
//! reads them. Recall is by meaning: memories are compared as vectors, so a question finds the
//! memory that answers it without sharing its words.
//!
//! # The four pieces
//!
//! * [`Store`] — the file. Memories, the vector index over them, and every change ever made,
//!   in one SQLite database with no service behind it.
//! * [`Embedder`] — what places text in the vector space. Part of a store's identity, not a
//!   swappable detail: a file written by one and searched by another answers wrongly and
//!   silently, so the store records which one wrote it and refuses the rest.
//! * [`Inference`] — what is worth remembering, and what it means for what is already
//!   remembered. [`Verbatim`] does neither and stores what it was given, which is a mode
//!   rather than a stub; [`Inferred`] is where a model does both.
//! * [`Mem`] — the command. Parses a line, resolves the store's name through the mount the
//!   call was handed, and answers in lines or JSON.
//!
//! [`Memory`] is the three middle ones working together, and is what [`Mem`] drives.
//!
//! # Why this crate needs a mount
//!
//! A store is a *file*, opened by SQLite through the host's own filesystem calls, and the name
//! it is called by — `notes.sqlite` — is a name in the workspace. Only a
//! [`Mount`](cortex::fs::Mount) turns one into the other, which is why
//! [`Executable::exec`](cortex::exec::Executable::exec) is handed one and why `mem`
//! refuses when there is none: with nothing mounted there is no file to open, and any path
//! substituted for it would be a different store that nobody asked about.

mod embed;
mod exec;
mod infer;
mod memory;
mod store;

pub use embed::*;
pub use exec::*;
pub use infer::*;
pub use memory::*;
pub use store::*;
