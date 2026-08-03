//! What every Cortex console server is built on — behind the `console_server`
//! feature.
//!
//! A *console server* is a process that accepts commands over stdio and runs
//! them somewhere — on this host, inside a micro-VM, wherever. What differs
//! between them is only that "somewhere"; the wire they speak and the loop they
//! serve it with are the same, and both live here. An implementation turns the
//! feature on, supplies a `handle` function, and is done.
//!
//! Keeping it in one place is the point: the parts a caller depends on — the
//! shape of a request, the shape of a reply, the one-in-one-out ordering — are
//! decided once, so two servers cannot drift apart on them. See [`Request`] and
//! [`Response`] for the wire, [`enter_loop`] for the loop.
//!
//! It is feature-gated because most of `cortex` is about what a backend *does*,
//! not about being spoken to over a pipe: a consumer that only wants
//! [`Executable`](crate::Executable) or [`volume`](crate::volume) has no use for
//! this module, and should not pay `serde_json` for it.

mod protocol;
#[cfg(feature = "console_server")]
mod server_util;

pub use protocol::*;
#[cfg(feature = "console_server")]
pub use server_util::*;
