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
//!   that drives a server, and the names a delegated call resolves to.
//!
//! Each end of a channel does one job, and a delegated executable does not make that
//! untrue. A server that needs one run says so in a [`Progress::Delegated`] — a response,
//! on the request the client is already waiting on — so execution runs both ways over one
//! channel that is only ever asked on from one side. [`Progress`] has the reasoning, and
//! what it costs.
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
