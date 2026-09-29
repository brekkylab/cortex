//! What the two ends of a console can *do* on a channel, independent of transport.
//!
//! A [`Client`](crate::protocol::Client) issues calls and takes answers back; a
//! [`Server`](crate::protocol::Server) takes what arrives and puts an answer out. Neither
//! decides what a message *means* (where a command runs, what a session allows, when
//! something has booted). The per-method wrappers follow
//! from `call` and `notify`, so they are written once here rather than per transport.
//!
//! [`stdio`](crate::protocol::stdio) is the current transport; another (e.g. a micro-VM's
//! virtio port) would change nothing here.
//!
//! # Every method hands back a boxed future
//!
//! An `async fn` in a trait returns a type a `dyn` cannot name, and a
//! [`ConsoleClient`](crate::console::ConsoleClient) holds a `dyn Client` so the transport
//! stays out of its type. A [`BoxFuture`](crate::BoxFuture) costs one allocation per call,
//! negligible against a pipe round trip; derived methods use the same shape.
//!
//! Futures and both ends are [`Send`] so a session can be handed to a task.
//!
//! # One call at a time
//!
//! `&mut self` throughout, with no `&self`-plus-lock: the protocol has one call
//! outstanding at a time, and a `read` interleaved with a running command is a question
//! the server cannot answer yet. The exclusive borrow checks this; for concurrency, open a
//! second console.

use crate::protocol::Error;

/// Why a call produced no result.
///
/// A refusal is about the call (retry or report it); a broken channel is about the
/// session, and every later call will fail the same way.
#[derive(Debug)]
pub enum Failure {
    /// The server answered `error`. Branch on [`code`](Error::code): only
    /// [`TIMED_OUT`](Error::TIMED_OUT) is worth retrying, with more time.
    Refused(Error),

    /// The channel or the server process failed; no answer will come.
    Broken(anyhow::Error),
}

impl Failure {
    /// The protocol code for a refusal; `None` for a broken channel.
    pub fn code(&self) -> Option<i64> {
        match self {
            Failure::Refused(error) => Some(error.code),
            Failure::Broken(_) => None,
        }
    }

    pub(crate) fn broken(what: impl Into<String>) -> Failure {
        Failure::Broken(anyhow::Error::msg(what.into()))
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Failure::Refused(error) => write!(f, "the console server refused: {error}"),
            Failure::Broken(e) => write!(f, "the console channel broke: {e}"),
        }
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Failure::Refused(error) => Some(error),
            Failure::Broken(e) => Some(e.as_ref()),
        }
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Failure {
        Failure::Refused(error)
    }
}
