//! Two halves. They share a vocabulary for describing a namespace, and no behaviour.
//!
//! * [`volume`] — expose any path-addressed store as a real filesystem. A store
//!   implements [`Mountable`](volume::Mountable) and a binding puts it in front of a
//!   concrete interface: a host FUSE mount, a guest microVM's virtio-fs, whatever else
//!   addresses files by path. `volume/ARCHITECTURE.md` has the long form.
//! * [`console`] — run commands somewhere else, over one JSON-RPC channel, with the
//!   executables only *this* side knows how to run reachable from inside that somewhere
//!   else. `console/ARCHITECTURE.md` has the long form.
//!
//! What they share is a crate, an error type, and one vocabulary:
//! [`Init`](console::Init) carries a [`WorkspaceSpec`](volume::WorkspaceSpec), and both ends
//! call [`Workspace::from_spec`](volume::Workspace::from_spec) to build their own live tree —
//! which is what makes a path mean the same thing to a command and to the delegated executable
//! it invoked.
//!
//! That is data, not behaviour, and the distinction is the arrangement: nothing in [`console`]
//! builds a workspace, calls [`Mountable`](volume::Mountable) or touches an adapter. **A
//! backend that projects a volume into its sandbox is built on top of both**, which is what
//! `cortex-local-console` and `cortex-uvm-console` are.
//!
//! # The file layout
//!
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
mod lock;
#[cfg(test)]
mod test_support;
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
