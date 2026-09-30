//! Node bindings for cortex, built with napi-rs.
//!
//! The API is cortex's own with camelCased names, so cortex's Rust docs apply: a
//! [`ConsoleClient`](cortex::console::ConsoleClient) is built by a builder and awaited, a
//! [`Directory`](cortex::fs::Directory) is assembled and handed to a host mount, a
//! [`Recipe`](cortex::image::Recipe) is a base plus steps, and an
//! [`ImageClient`](cortex::image::ImageClient) builds images ahead of the sessions that run on them.
//!
//! Modules mirror the crate: [`console`] runs commands, [`fs`] builds the trees they see,
//! [`image`] covers what they run on and the client that builds it, [`error`] maps failures
//! to JavaScript errors, and `ensure` fetches the console server.
//!
//! Every call that waits returns a `Promise` settled on napi's tokio runtime: a stdio client
//! spawns its server and reads from it, which needs a reactor. `ConsoleClient` and `ImageClient`
//! keep their Rust client in an `Arc<Mutex<Option<..>>>` beside the handle of the runtime it
//! started on. Each call clones the `Arc` into its `'static` future, and the lock makes calls
//! take turns on the one channel, as `&mut self` does in Rust: a second `exec` waits for the
//! first rather than interleaving with it. The Rust client's `Drop` says `quit` only on a
//! runtime and a garbage-collection finalizer runs off one, so the last holder drops it inside
//! the kept runtime: a collected client ends the same way as a closed one, and `close()` only
//! picks the line where it ends.
//!
//! The modules are public for a binding that links this crate into its own addon (the `rlib` in
//! `Cargo.toml`). Linking is enough: napi registers every class here into whichever addon links it.

pub mod console;
#[cfg(feature = "ensure")]
pub mod ensure;
pub mod error;
pub mod fs;
pub mod image;
