//! Framed messages over a pipe.
//!
//! - [`StdioClient`] starts the server process it drives.
//! - [`StdioServer`] answers on stdin and stdout.
//! - [`read`](fn@read) and [`write`](fn@write) move one frame over any descriptor (pipe,
//!   virtio port, `Vec<u8>`).
//!
//! ```text
//! [u32 len][serialized Message]
//! ```
//!
//! A length prefix means no delimiter to search for, and so none a payload could forge.
//!
//! # The two directions are not paired
//!
//! [`write`](fn@write) and [`read`](fn@read) each take one descriptor. A pipe pair is two
//! independent streams sharing only the framing, and a struct holding both would need a
//! type parameter per direction and would block borrowing both halves at once (write a
//! request, then read its answer). Each end keeps whichever halves it has as fields, and
//! may buffer its incoming one: the protocol owns it for the session, so reading ahead
//! takes no one else's bytes.
//!
//! A command's own stdio never appears here; it is captured where the command runs and
//! returned inside an [`ExecResp`](crate::protocol::ExecResp), so one descriptor pair suffices.
//!
//! # Neither is cancel-safe
//!
//! Dropping either future mid-await leaves the descriptor mid-frame (a header without its
//! payload, or a payload half consumed), undetectably and unrecoverably. So neither goes
//! in a [`select!`](tokio::select) branch; wait on other things around a frame, as
//! [`StdioClient::call`](StdioClient#method.call) does by owning its descriptors for the
//! whole round trip.

mod channel;
mod client;
mod server;

pub use channel::*;
pub use client::*;
pub use server::*;
