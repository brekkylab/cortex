//! Node bindings for cortex, built with napi-rs.
//!
//! The API is cortex's own with camelCased names, so cortex's Rust docs apply: a
//! [`ConsoleClient`](cortex::console::ConsoleClient) is built by a builder and awaited, a
//! [`Directory`](cortex::fs::Directory) is assembled and handed to a host mount, a
//! [`Recipe`](cortex::image::Recipe) is a base plus steps, and an
//! [`ImageClient`](cortex::image::ImageClient) builds images ahead of the sessions that run on them.
//!
//! Modules mirror the crate: [`console`] runs commands, [`fs`] builds the trees they see,
//! [`image`] covers what they run on and the client that builds it, and [`error`] maps failures
//! to JavaScript errors.
//!
//! The modules are public for a binding that links this crate into its own addon (the `rlib` in
//! `Cargo.toml`). Linking is enough: napi registers every class here into whichever addon links it.

pub mod console;
#[cfg(feature = "ensure")]
pub mod ensure;
pub mod error;
pub mod fs;
pub mod image;
