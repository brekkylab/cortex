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
//! - [`ConsoleClient`] — the public end, and what a caller normally reaches for: the channel
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
mod message;
pub mod stdio;

use std::io;

pub use base::*;
use futures_core::future::BoxFuture;
pub use message::*;

/// The asking end of a channel: issue a call, get its answer back.
///
/// Two things are a transport's: putting a call on the wire and coming back with the
/// response that answers *that* call, and putting a notification on the wire. The
/// rest of the methods below are neither, so they are written once here — which is also
/// the one place a [`Response`](crate::console::Response) becomes what its method returns.
pub trait Client: Send {
    /// Make one call and wait for its response.
    ///
    /// Allocating the id and pairing it with what comes back is the transport's, because
    /// so is anything else that may arrive while it waits — and the number is a fact about
    /// the wire and not about the answer, which is why it does not come back out.
    ///
    /// What comes back is a [`Response`](crate::console::Response), typed by the `method`
    /// the frame echoed — so this end reads an answer without holding a table of what it
    /// asked. Turning it into the one variant a given method returns is the derived
    /// methods' below.
    ///
    /// An [`Error`](crate::console::Response::Error) arrives as
    /// [`Refused`](Failure::Refused) rather than as a `Response`, because a caller that got
    /// no answer has one thing to handle and not two shapes of it — and which of the two
    /// happened is what `Failure` says.
    ///
    /// Dropping this future part way is dropping the session with it. The call may
    /// already be on the wire, and a transport that owns a descriptor for the length of
    /// a round trip has no way to put back what it half read — so a caller that wants to
    /// stop waiting stops using the client too. A [`timeout`](ExecCall::timeout_ms) is what
    /// bounds an execution; cancelling the wait for one is not.
    fn call(&mut self, call: Call) -> BoxFuture<'_, Result<Response, Failure>>;

    /// Send something nothing answers, so there is nothing to wait for.
    fn notify(&mut self, notification: Notification) -> BoxFuture<'_, Result<(), Failure>>;

    /// Which protocol version the server speaks.
    fn version(&mut self) -> BoxFuture<'_, Result<VersionResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::Version(VersionCall {})).await? {
                Response::Version(answer) => Ok(answer),
                other => Err(mismatched(Method::Version, other)),
            }
        })
    }

    /// Build a recipe, and hear the ref and digest it is stored under.
    ///
    /// Needs no session, so it may be called with or without an [`init`](Self::init).
    fn build_image(
        &mut self,
        build: BuildImageCall,
    ) -> BoxFuture<'_, Result<BuildImageResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::BuildImage(build)).await? {
                Response::BuildImage(answer) => Ok(answer),
                other => Err(mismatched(Method::BuildImage, other)),
            }
        })
    }

    /// Forget a built image. Needs no session.
    fn remove_image(
        &mut self,
        remove: RemoveImageCall,
    ) -> BoxFuture<'_, Result<RemoveImageResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::RemoveImage(remove)).await? {
                Response::RemoveImage(answer) => Ok(answer),
                other => Err(mismatched(Method::RemoveImage, other)),
            }
        })
    }

    /// Every image the server has built. Needs no session.
    fn list_images(&mut self) -> BoxFuture<'_, Result<ListImagesResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::ListImages(ListImagesCall {})).await? {
                Response::ListImages(answer) => Ok(answer),
                other => Err(mismatched(Method::ListImages, other)),
            }
        })
    }

    /// Say what this session is: the tree it works in, and what its commands run in and
    /// may reach.
    ///
    /// Nothing is booted or mounted by it. What returning means is that there is a server
    /// on the far end, that it speaks this protocol, and that it has what it was told —
    /// which is the only thing about a session a client can hear before it asks for work.
    ///
    /// What comes back is where that tree will be: an [`InitResp`], carrying the path
    /// every later `read` and `write` is spelled in. Whether it is the path that was asked
    /// for is the server's to decide, which is why it is answered rather than assumed.
    fn init(&mut self, init: InitCall) -> BoxFuture<'_, Result<InitResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::Init(init)).await? {
                Response::Init(answer) => Ok(answer),
                other => Err(mismatched(Method::Init, other)),
            }
        })
    }

    /// Run one command, and return everything it produced.
    ///
    /// An `Ok` is the whole execution; a command that merely failed is an `Ok` with a
    /// non-zero [`code`](ExecResp::code), and a [`Refused`](Failure::Refused) is the
    /// execution having no result at all — it timed out, or there was nothing there to run.
    fn exec(&mut self, exec: ExecCall) -> BoxFuture<'_, Result<ExecResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::Exec(exec)).await? {
                Response::Exec(answer) => Ok(answer),
                other => Err(mismatched(Method::Exec, other)),
            }
        })
    }

    /// Read part of a file where the executor runs things.
    ///
    /// One message holds the answer, so a file larger than that comes back in pieces:
    /// [`size`](ReadResp::size) against what arrived says whether there are more,
    /// and a further `read` from further along is how to get them.
    fn read(&mut self, read: ReadCall) -> BoxFuture<'_, Result<ReadResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::Read(read)).await? {
                Response::Read(answer) => Ok(answer),
                other => Err(mismatched(Method::Read, other)),
            }
        })
    }

    /// Put bytes in a file where the executor runs things, and hear how big it is
    /// afterwards.
    fn write(&mut self, write: WriteCall) -> BoxFuture<'_, Result<WriteResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::Write(write)).await? {
                Response::Write(answer) => Ok(answer),
                other => Err(mismatched(Method::Write, other)),
            }
        })
    }

    /// Take everything this session has written, as a blob another can start on.
    ///
    /// The other half of [`InitCall::snapshot`]: what comes back is exactly what that
    /// takes, so a session is carried on by opening a new one with this in hand. What is
    /// in it is the far end's business — it is that executor's own encoding of the changes,
    /// read back only by an executor of the same kind — and a caller's part is to keep the
    /// bytes and hand them over.
    ///
    /// One message holds the answer, unlike [`read`](Self::read), which comes back in
    /// pieces. A snapshot cannot: it is read back as a filesystem, and the front of one is
    /// not a smaller session but a broken tree — so a session that has written more than a
    /// message holds is refused rather than shortened.
    fn snapshot(&mut self) -> BoxFuture<'_, Result<SnapshotResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::Snapshot(SnapshotCall {})).await? {
                Response::Snapshot(answer) => Ok(answer),
                other => Err(mismatched(Method::Snapshot, other)),
            }
        })
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
    /// The other half of [`start`](Self::start)'s trade: a guest, a mounted tree and a
    /// scratch directory are memory, descriptors and disk held on the far end, and the next call
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

/// A peer that answered one method with another's result.
///
/// Unreachable against a peer that is keeping its place — one call is outstanding, and the
/// transport already dropped anything not carrying its `id` — so this is a peer that has
/// lost track of itself and there is nothing usable either way. That is what
/// [`INTERNAL_ERROR`](Error::INTERNAL_ERROR) is for, and the reason belongs in a log.
fn mismatched(wanted: Method, got: Response) -> Failure {
    Failure::from(Error::new(
        Error::INTERNAL_ERROR,
        match got.method() {
            Some(answered) => format!("asked {wanted} and was answered a {answered} result"),
            None => format!("asked {wanted} and was answered nothing this end can read"),
        },
    ))
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
    ///
    /// One value and not a `Result` around one: a
    /// [`Response`](crate::console::Response) is the answer *or* the
    /// [`Error`](crate::console::Response::Error) saying there is none, which is the whole
    /// of what a response can be — see [`Response`](crate::console::Response).
    ///
    /// It is also what a backend that *relays* one hands straight on. A response names its
    /// own method, so a relay never has to re-type an answer it did not compose.
    fn respond(&mut self, id: RequestId, result: Response) -> BoxFuture<'_, io::Result<()>>;
}
