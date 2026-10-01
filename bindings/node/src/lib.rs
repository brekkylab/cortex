//! Node bindings for virtx, built with napi-rs.
//!
//! The API is virtx's own with camelCased names, so virtx's Rust docs apply. Modules mirror
//! the crate: [`console`] runs commands in a [`ConsoleClient`](virtx::console::ConsoleClient)
//! built by a builder and awaited, [`fs`] assembles a [`Directory`](virtx::fs::Directory) for
//! a host mount, [`image`] covers a [`Recipe`](virtx::image::Recipe) (a base plus steps) and
//! the [`ImageClient`](virtx::image::ImageClient) that builds it ahead of a session, [`error`]
//! maps failures to JavaScript errors, and `ensure` fetches the console server.
//!
//! Every call that waits returns a `Promise` settled on napi's tokio runtime, since a stdio
//! client spawns and reads its server, which needs a reactor. `ConsoleClient` and `ImageClient`
//! keep their Rust client in an `Arc<Mutex<Option<..>>>` beside the handle of the runtime it
//! started on: each call clones the `Arc` into its `'static` future, and the lock makes calls
//! take turns on the one channel, as `&mut self` does in Rust. The Rust client's `Drop` says
//! `quit` only on a runtime, and a garbage-collection finalizer runs off one, so the last holder
//! drops it inside the kept runtime: a collected client ends like a closed one, and `close()`
//! only picks when.
//!
//! The modules are public for a binding that links this crate into its own addon (the `rlib` in
//! `Cargo.toml`). Linking is enough: napi registers every class here into whichever addon links it.

pub mod console;
#[cfg(feature = "ensure")]
pub mod ensure;
pub mod error;
pub mod fs;
pub mod image;
