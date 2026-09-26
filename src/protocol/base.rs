//! What the two ends of a console can *do* on a channel, apart from whatever carries
//! them.
//!
//! One end asks and the other answers, and the two ends are named for which they are:
//! a [`Client`] issues calls and takes the answers back, a [`Server`] takes
//! what arrives and puts an answer out. Both are about moving messages. Neither decides
//! what a message *means* — where a command runs, what a session allows, when something
//! has booted: none of that is here, and a transport is the last place it should be.
//!
//! Each end is only ever the one it is. There is no request a server issues, so nothing
//! here has to be both — no pending table on the answering side, and no listener on the
//! asking one.
//!
//! What is here is only what would otherwise be written once per transport: `init`,
//! `exec`, `read`, `write`, `start`, `stop` and `quit` follow from `call` and `notify`,
//! so they follow once.
//!
//! [`stdio`](crate::console::stdio) is the transport there is — framed JSON-RPC
//! over a pipe. A micro-VM's virtio port would be another, and nothing here would
//! change.
//!
//! # Every method hands back a boxed future
//!
//! Waiting is what these two do — for a pipe to drain, for a server to answer, for a
//! command that has not finished — so every method is something to `await`, and none of
//! them is an `async fn`. An `async fn` in a trait returns a type only the implementation
//! knows, which is a type a `dyn` cannot name: it would make both traits unusable behind
//! a pointer, and a [`ConsoleClient`](crate::console::ConsoleClient) holds a `dyn Client` precisely so
//! that which transport it drives is not in its type.
//!
//! So each method returns a [`BoxFuture`] instead — one allocation per call, against a
//! round trip over a pipe — and the derived methods are written the same way as the two
//! they are derived from, rather than being a second shape to read.
//!
//! [`Send`], because a session is a thing to hand to a task. Every future here can cross
//! threads, which is also why both traits require it of the ends themselves.
//!
//! # One call at a time, and the borrow that says so
//!
//! `&mut self` throughout. Nothing here is `&self` with a lock behind it, and that is the
//! protocol showing through rather than an omission: there is one call outstanding at a
//! time, an id is allocated per call, and a second caller interleaving a `read` while a
//! command runs would be asking a question the server has no way to answer yet.
//!
//! An exclusive borrow is how that is said in a signature, and it is checked rather than
//! documented. A caller wanting concurrency wants a second console.

use crate::protocol::Error;

/// Why a call produced no result.
///
/// The distinction is the useful part. A refusal came from the server and is about
/// the call — retry it with more time, or report it. A broken channel is about the
/// session, and every later call will fail the same way.
#[derive(Debug)]
pub enum Failure {
    /// The server answered `error`. Branch on [`code`](Error::code) —
    /// [`TIMED_OUT`](Error::TIMED_OUT) is worth another try with more time, the rest
    /// are not.
    Refused(Error),

    /// The channel or the server process failed, so there is no answer and will not
    /// be one.
    Broken(anyhow::Error),
}

impl Failure {
    /// The protocol code, for a refusal. `None` for a broken channel, which is not
    /// something the server said.
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
