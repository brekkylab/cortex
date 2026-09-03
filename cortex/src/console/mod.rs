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
//! - `base` — what each end can *do* on a channel: a [`Client`] only asks, a
//!   [`Server`] only answers. Both move messages and neither reads a meaning into
//!   one.
//! - [`stdio`] — the transport there is: framed JSON-RPC over a pipe, and the two
//!   ends over it ([`StdioClient`](stdio::StdioClient), which starts the server process
//!   it drives, and [`StdioServer`](stdio::StdioServer), which answers on stdin and
//!   stdout).
//! - [`Console`] — the public end, and what a caller normally reaches for: the channel
//!   that drives a server, and the session it was opened with.
//!
//! Everything that waits is a future. A console spends nearly all of its time waiting —
//! on a pipe, on a command, on a backend bringing a kernel up — so a caller with several
//! of them drives them all from one runtime, and every method that could wait is
//! something to `await`. What is *not* concurrent is a single session: see [`Client`] for
//! the borrow that says so.
//!
//! Each end of a channel does one job and only that one. There is no request a server
//! issues, so nothing on either side needs a pending table, a listener, or a reader that
//! must not block on work only it can unblock.
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
