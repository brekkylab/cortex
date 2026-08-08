//! A module here is a directory whose `mod.rs` holds the module's own documentation and
//! its re-exports, and whose siblings hold the code — including one named after the
//! module itself, for the type the module exists for: `console/console.rs` has
//! [`Console`](console::Console), `message/message.rs` has
//! [`Message`](console::Message).
//!
//! That is what `module_inception` fires on, and it is the arrangement rather than an
//! accident of one file: the reasoning for a module is long here and belongs somewhere
//! that is not also holding a type, and a reader looking for `Console` should find it in
//! a file with that name.
#![allow(clippy::module_inception)]

pub mod console;
mod error;
pub mod executable;
pub mod volume;

pub use error::*;

/// What every method that waits hands back — a [`Client`], a [`Server`], an
/// [`Executable`].
///
/// Re-exported because implementing any of those three means writing the type, and a
/// caller should not have to take a dependency of ours to say what our own traits
/// return. See [`console::base`](console) for why the futures are boxed rather than
/// written as `async fn`.
///
/// [`Client`]: console::Client
/// [`Server`]: console::Server
/// [`Executable`]: executable::Executable
pub use futures_core::future::BoxFuture;
