//! What the two ends of a console can *do* on a channel, apart from whatever carries
//! them.
//!
//! One end asks and the other answers, and the two ends are named for which they are:
//! a [`Client`] issues calls and takes the answers back, a [`Server`] takes
//! what arrives and puts an answer out. Both are about moving messages. Neither decides
//! what a message *means* — where a command runs, what a session allows, when something
//! has booted: none of that is here, and a transport is the last place it should be.
//!
//! Each end is only ever the one it is. Delegation does not change that: a server needing
//! something from the client says so in a [`Progress::Delegated`] — a response, on the
//! request the client is already waiting on — so nothing here has to be both.
//!
//! What is here is only what would otherwise be written once per transport: `start`,
//! `exec`, `resume`, `stop` and `quit` follow from `call` and `notify`, so they follow
//! once.
//!
//! [`stdio`](crate::console::stdio) is the transport there is — framed JSON-RPC
//! over a pipe. A micro-VM's virtio port would be another, and nothing here would
//! change.

use std::io;

use crate::console::{
    Call, Error, Exec, Message, Notification, Outcome, Progress, RequestId, Start,
};

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

/// The asking end of a channel: issue a call, get its answer back.
///
/// Two things are a transport's: putting a call on the wire and coming back with the
/// response that answers *that* call, and putting a notification on the wire. The
/// four methods below are neither, so they are written once here — which is also the
/// one place an untyped [`Outcome`] becomes what its method returns.
pub trait Client {
    /// Make one call and wait for its response.
    ///
    /// Allocating the id and pairing it with what comes back is the transport's,
    /// because so is anything else that may arrive while it waits.
    fn call(&mut self, call: Call) -> Result<Outcome, Failure>;

    /// Send something nothing answers, so there is nothing to wait for.
    fn notify(&mut self, notification: Notification) -> Result<(), Failure>;

    /// Boot the server, and make the delegated names runnable inside it.
    ///
    /// Returning is the readiness signal: booting is not free, and this is where it
    /// is paid for rather than inside the first command.
    fn start(&mut self, start: Start) -> Result<(), Failure> {
        self.call(Call::Start(start))?.take().map_err(Failure::from)
    }

    /// Run one command, and return how far it got.
    ///
    /// [`Done`](Progress::Done) is the whole execution; a command that merely failed is
    /// an `Ok` with a non-zero [`code`](crate::console::ExecResult::code), and a
    /// [`Refused`](Failure::Refused) is the execution having no result at all.
    ///
    /// [`Delegated`](Progress::Delegated) is the execution pausing on a name whose
    /// behaviour lives out here, and it has to be answered with
    /// [`resume`](Self::resume) before anything else is asked. Resolving those against
    /// something that knows what a name *does* is not a transport's job — that is
    /// [`Console::exec`](crate::console::Console::exec), which is what a caller
    /// normally wants.
    fn exec(&mut self, exec: Exec) -> Result<Progress, Failure> {
        self.call(Call::Exec(exec))?.take().map_err(Failure::from)
    }

    /// Say how the delegated call the last response asked for ended, and carry on.
    ///
    /// Only ever a reply to a [`Delegated`](Progress::Delegated), and the answer is the
    /// next [`Progress`] of the same execution — another delegated call, or the end of
    /// it.
    fn resume(&mut self, outcome: Outcome) -> Result<Progress, Failure> {
        self.call(Call::Resume(outcome))?
            .take()
            .map_err(Failure::from)
    }

    /// Release what [`start`](Self::start) booted. Another `start` is allowed after
    /// it.
    fn stop(&mut self) -> Result<(), Failure> {
        self.call(Call::Stop)?.take().map_err(Failure::from)
    }

    /// Say the session is over.
    fn quit(&mut self) -> Result<(), Failure> {
        self.notify(Notification::Quit)
    }
}

/// The answering end of a channel: take what arrived, put an answer out.
///
/// Both halves of that are a transport's, and there is nothing else. What a message
/// means, where a command runs, what a session allows when — none of it is here. This
/// trait moves frames and holds no state.
pub trait Server {
    /// The next message. `Ok(None)` is the other end closing the channel cleanly.
    fn recv(&mut self) -> io::Result<Option<Message>>;

    /// Put out one response, with the id of the request it answers.
    fn respond(&mut self, id: RequestId, outcome: Outcome) -> io::Result<()>;
}
