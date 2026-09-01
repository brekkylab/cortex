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
//! a pointer, and a [`Console`](crate::console::Console) holds a `dyn Client` precisely so
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
//! protocol showing through rather than an omission: an id is allocated per call, a
//! delegated execution owes an answer before anything else may be asked, and a second
//! caller interleaving a `read` into the middle of that chain would be asking a question
//! the server has no way to answer.
//!
//! An exclusive borrow is how that is said in a signature, and it is checked rather than
//! documented. A caller wanting concurrency wants a second console.

use std::io;

use futures_core::future::BoxFuture;

use crate::console::{
    Call, Commit, CommitResult, Error, Exec, Init, InitResult, Message, Notification, Outcome,
    Progress, Read, ReadResult, RequestId, Write, WriteResult,
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
/// rest of the methods below are neither, so they are written once here — which is also
/// the one place an untyped [`Outcome`] becomes what its method returns.
pub trait Client: Send {
    /// Make one call and wait for its response, and say which id it went out under.
    ///
    /// Allocating the id and pairing it with what comes back is the transport's, because
    /// so is anything else that may arrive while it waits. Handing the number back is
    /// what lets a caller quote a request it has made: a delegated call is answered by
    /// another `exec` naming the one it carries on from
    /// ([`ExecCmd::Resume`](crate::console::ExecCmd::Resume)), and the end that resolves a
    /// delegated name is above the transport that numbered it.
    ///
    /// Outside the `Result`, because an id is spent either way — the request may well be
    /// on the wire when the answer to it never comes — so it is there to be reported with
    /// whichever of the two arrives.
    ///
    /// Dropping this future part way is dropping the session with it. The call may
    /// already be on the wire, and a transport that owns a descriptor for the length of
    /// a round trip has no way to put back what it half read — so a caller that wants to
    /// stop waiting stops using the client too. A [`timeout`](Exec::timeout_ms) is what
    /// bounds an execution; cancelling the wait for one is not.
    fn call(&mut self, call: Call) -> BoxFuture<'_, (RequestId, Result<Outcome, Failure>)>;

    /// Send something nothing answers, so there is nothing to wait for.
    fn notify(&mut self, notification: Notification) -> BoxFuture<'_, Result<(), Failure>>;

    /// Say what this session is: the names it may call back into, and the tree it works in.
    ///
    /// Nothing is booted or mounted by it. What returning means is that there is a server
    /// on the far end, that it speaks this protocol, and that it has what it was told —
    /// which is the only thing about a session a client can hear before it asks for work.
    ///
    /// What comes back is where that tree will be: an [`InitResult`], carrying the path
    /// every later `read`, `write` and reported [`cwd`](Exec::cwd) is spelled in. Whether
    /// it is the path that was asked for is the server's to decide, which is why it is
    /// answered rather than assumed.
    fn init(&mut self, init: Init) -> BoxFuture<'_, Result<InitResult, Failure>> {
        Box::pin(async move { answered(self.call(Call::Init(init)).await).1 })
    }

    /// Run one command, and return how far it got.
    ///
    /// [`Done`](Progress::Done) is the whole execution; a command that merely failed is
    /// an `Ok` with a non-zero [`code`](crate::console::ExecResult::code), and a
    /// [`Refused`](Failure::Refused) is the execution having no result at all.
    ///
    /// [`Delegated`](Progress::Delegated) is the execution pausing on a name whose
    /// behaviour lives out here, and it has to be answered before anything else is
    /// asked — by another `exec` carrying what the name produced as an
    /// [`ExecCmd::Resume`](crate::console::ExecCmd::Resume). Resolving those against
    /// something that knows what a name
    /// *does* is not a transport's job — that is
    /// [`Console::exec`](crate::console::Console::exec), which is what a caller
    /// normally wants.
    ///
    /// The [`RequestId`] comes back with it because this is the one method whose caller
    /// has to quote it: answering a `Delegated` means naming the request that got it.
    fn exec(&mut self, exec: Exec) -> BoxFuture<'_, (RequestId, Result<Progress, Failure>)> {
        Box::pin(async move { answered(self.call(Call::Exec(exec)).await) })
    }

    /// Read part of a file where the executor runs things.
    ///
    /// One message holds the answer, so a file larger than that comes back in pieces:
    /// [`size`](ReadResult::size) against what arrived says whether there are more,
    /// and a further `read` from further along is how to get them.
    fn read(&mut self, read: Read) -> BoxFuture<'_, Result<ReadResult, Failure>> {
        Box::pin(async move { answered(self.call(Call::Read(read)).await).1 })
    }

    /// Put bytes in a file where the executor runs things, and hear how big it is
    /// afterwards.
    fn write(&mut self, write: Write) -> BoxFuture<'_, Result<WriteResult, Failure>> {
        Box::pin(async move { answered(self.call(Call::Write(write)).await).1 })
    }

    /// Keep what this session has written, as a base a later session can name.
    ///
    /// The one call whose answer outlives the session: everything else here is about work
    /// inside one. A server with nothing to keep — one whose commands run on its own
    /// filesystem — refuses it, and so does a session that did not say it might.
    fn commit(&mut self, commit: Commit) -> BoxFuture<'_, Result<CommitResult, Failure>> {
        Box::pin(async move { answered(self.call(Call::Commit(commit)).await).1 })
    }

    /// Boot now, to hide the cold start.
    ///
    /// Optional: [`exec`](Self::exec), [`read`](Self::read) and [`write`](Self::write)
    /// are served by a server that boots one if there is none, so this unlocks nothing
    /// and only moves who waits for the boot — off the first command and onto whatever
    /// the caller is doing between here and there.
    ///
    /// `Ok` is the notification having gone out and not the server having booted, so a
    /// boot that fails is heard as a [`BOOT_FAILED`](Error::BOOT_FAILED) on the next call
    /// that needed one.
    fn start(&mut self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async move { self.notify(Notification::Start).await })
    }

    /// Release what booting took, to stop occupying it while nothing is running.
    ///
    /// The other half of [`start`](Self::start)'s trade: a guest, a socket and a scratch
    /// directory are memory, descriptors and disk held on the far end, and the next call
    /// that needs a booted session boots one — so handing them back costs one boot later
    /// and nothing else.
    fn stop(&mut self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async move { self.notify(Notification::Stop).await })
    }

    /// Say the session is over.
    fn quit(&mut self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async move { self.notify(Notification::Quit).await })
    }
}

/// What a [`call`](Client::call) came to, as the type the method returns.
///
/// The half of a derived method that is the same for all of them: an [`Outcome`] is
/// untyped on the wire because a `result` is typed by the method its `id` was issued for,
/// and each method above is the one place that knows which type that is. A `result` that
/// will not deserialize becomes an [`INTERNAL_ERROR`](Error::INTERNAL_ERROR) — see
/// [`Outcome::take`].
///
/// The id passes through untouched, so a method that has to report which call this was
/// still can.
fn answered<T: serde::de::DeserializeOwned>(
    (id, outcome): (RequestId, Result<Outcome, Failure>),
) -> (RequestId, Result<T, Failure>) {
    (id, outcome.and_then(|o| o.take().map_err(Failure::from)))
}

/// The answering end of a channel: take what arrived, put an answer out.
///
/// Both halves of that are a transport's, and there is nothing else. What a message
/// means, where a command runs, what a session allows when — none of it is here. This
/// trait moves frames and holds no state.
pub trait Server: Send {
    /// The next message. `Ok(None)` is the other end closing the channel cleanly.
    ///
    /// Not cancel-safe: a dropped `recv` may have taken part of a frame off the
    /// descriptor and has nowhere to put it back. An end waiting on something besides
    /// the channel waits on it between messages rather than instead of one.
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<Message>>>;

    /// Put out one response, with the id of the request it answers.
    fn respond(&mut self, id: RequestId, outcome: Outcome) -> BoxFuture<'_, io::Result<()>>;
}
