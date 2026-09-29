//! Node bindings for cortex, built with napi-rs.
//!
//! The shape is cortex's own, spelled in JavaScript: a [`ConsoleClient`](cortex::console::ConsoleClient)
//! is built by a builder and awaited, a [`Directory`](cortex::fs::Directory) is assembled
//! and handed to a host mount, a [`Recipe`](cortex::image::Recipe) is a base and its steps,
//! and an [`ImageClient`](cortex::image::ImageClient) builds one ahead of the session that
//! runs on it. Names are camelCased and nothing else changes — a caller reading cortex's Rust
//! documentation should find the same names doing the same things.
//!
//! One module per half, as in the crate: [`console`] for running commands, [`fs`] for the
//! trees they see, [`image`] for what they run on and the client that builds it, and [`error`] for how either half's
//! failures arrive as JavaScript errors.

//!
//! The modules are public for a binding that links this crate into an addon of its own — see
//! the `rlib` in `Cargo.toml`. Linking it is all that takes: napi registers every class here
//! into whichever addon it is linked into.

pub mod console;
#[cfg(feature = "ensure")]
pub mod ensure;
pub mod error;
pub mod fs;
pub mod image;
