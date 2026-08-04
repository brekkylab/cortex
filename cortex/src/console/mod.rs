//! The two ends of a console, and the wire between them.
//!
//! A *console server* is a process that speaks JSON-RPC over stdio and runs the
//! commands it is asked to run somewhere — on this host, inside a micro-VM,
//! wherever. A *console client* is whoever drives one. What differs between
//! servers is only that "somewhere"; the methods they answer, which end may ask
//! what, and how a request is paired with its response are decided once, here, so
//! two servers cannot drift apart on them.
//!
//! - `message` — [`Message`] and the methods, errors and payloads both ends agree on.
//! - `base` — what each end can *do* on a channel: a [`Requestable`] only asks, a
//!   [`Responsable`] only answers. Both move messages and neither reads a meaning into
//!   one.
//! - [`stdio`] — the transport there is: framed JSON-RPC over a pipe, and the two
//!   ends over it ([`StdioRequester`](stdio::StdioRequester),
//!   [`StdioResponder`](stdio::StdioResponder)).
//! - [`Console`] — the public end, and what a caller normally reaches for: the channel
//!   that drives a server, the channels delegated calls arrive on, and the names those
//!   calls resolve to.
//!
//! Each end of a channel does one job. A delegated executable is what used to make that
//! untrue — the server asked and the client answered, on this same channel — and it no
//! longer does: a shim reaches the client on a channel of its own, so execution still
//! runs both ways without any channel being used both ways. [`Console`] has the
//! reasoning.
//!
//! [`Message`] has the reasoning for the protocol,
//! [`read`](stdio::read) and [`write`](stdio::write) for the wire.

mod base;
mod console;
mod message;
pub mod stdio;

pub use base::*;
pub use console::*;
pub use message::*;
