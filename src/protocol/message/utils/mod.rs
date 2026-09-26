//! What the wire needs that is nobody's method and nobody's shape.
//!
//! Two things, and what they have in common is the reason they are not in the file that
//! uses them: each is a *codec* concern that more than one of the three shapes runs into.
//!
//! - `bytes` — a byte payload, carried as itself. Reached from both halves of the
//!   exchange: an [`ExecResp`](super::ExecResp)'s output and a
//!   [`ReadResp`](super::ReadResp)'s data going one way, a
//!   [`WriteCall`](super::WriteCall)'s the other.
//! - `flatten` — writing a value's own members into an object somebody else opened,
//!   which is what lets a [`Call`](super::Call) and a [`Response`](super::Response) each
//!   declare its shape and still land beside `jsonrpc` rather than under a member of its
//!   own.
//!
//! Neither is protocol. Which methods exist, what each carries and which members decide a
//! message's shape are all said elsewhere; these two are how what was said gets written
//! down, and a reader after the *protocol* never has to open either.

pub(super) mod bytes;
pub(super) mod flatten;
