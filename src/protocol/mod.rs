//! The two ends of a console, and the wire between them.
//!
//! A *console server* speaks JSON-RPC over stdio and runs commands somewhere: this
//! host, a micro-VM, wherever. A *console client* drives one. Servers differ only in
//! that "somewhere"; the methods, which end may ask what, and request/response pairing
//! are defined once here so servers cannot drift apart on them.
//!
//! - `message` — [`Message`] and the methods, errors and payloads both ends agree on.
//! - `base` — what each end does on a channel: a [`Client`] only asks, a [`Server`]
//!   only answers. Neither reads meaning into a message.
//! - [`stdio`] — framed JSON-RPC over a pipe, with [`StdioClient`](stdio::StdioClient)
//!   (starts the server process it drives) and [`StdioServer`](stdio::StdioServer)
//!   (answers on stdin and stdout).
//! - [`ConsoleClient`](crate::console::ConsoleClient) — the public end: the channel driving a server, plus the session
//!   it was opened with.
//!
//! Everything that waits is a future, so one runtime can drive many consoles. A single
//! session is not concurrent: see [`Client`].
//!
//! A server never issues a request, so neither end needs a pending table, a listener, or
//! a reader that must not block on work only it can unblock.
//!
//! [`Message`] has the protocol's rationale, [`read`](stdio::read) and
//! [`write`](stdio::write) the wire's.

mod base;
mod message;
pub mod stdio;

use std::io;

pub use base::*;
use futures_core::future::BoxFuture;
pub use message::*;

/// The asking end of a channel: issue a call, get its answer back.
///
/// A transport implements only [`call`](Self::call) and [`notify`](Self::notify); the
/// per-method wrappers are derived here, which is also the one place a
/// [`Response`] becomes what its method returns.
pub trait Client: Send {
    /// Make one call and wait for its response.
    ///
    /// The transport allocates the id and pairs the response with it; the id is a wire
    /// detail, so it is not returned. The [`Response`] is typed
    /// by the `method` the frame echoed, so no table of outstanding asks is needed.
    ///
    /// An [`Error`](crate::protocol::Response::Error) arrives as
    /// [`Refused`](Failure::Refused), so a caller handles "no answer" in one shape.
    ///
    /// Not cancel-safe: dropping this future mid-call may leave a half-read frame, so the
    /// client is unusable afterwards. Bound an execution with
    /// [`timeout_ms`](ExecCall::timeout_ms), not by cancelling the wait.
    fn call(&mut self, call: Call) -> BoxFuture<'_, Result<Response, Failure>>;

    /// Send a notification; nothing answers it.
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

    /// Build a recipe; returns the ref and digest it is stored under.
    ///
    /// Needs no session, so [`init`](Self::init) is optional.
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

    /// Describe this session: the tree it works in, and what its commands run in and may
    /// reach.
    ///
    /// Boots and mounts nothing. Returning means a server is there, speaks this protocol,
    /// and accepted the description.
    ///
    /// The [`InitResp`] carries the path later `read`s and `write`s are spelled in; the
    /// server decides it, so it may differ from the one asked for.
    fn init(&mut self, init: InitCall) -> BoxFuture<'_, Result<InitResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::Init(init)).await? {
                Response::Init(answer) => Ok(answer),
                other => Err(mismatched(Method::Init, other)),
            }
        })
    }

    /// Run one command and return everything it produced.
    ///
    /// A command that failed is still `Ok`, with a non-zero [`code`](ExecResp::code).
    /// [`Refused`](Failure::Refused) means no result at all: it timed out, or there was
    /// nothing to run.
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
    /// A file larger than one message comes back in pieces: compare
    /// [`size`](ReadResp::size) with what arrived, and `read` again from further along.
    fn read(&mut self, read: ReadCall) -> BoxFuture<'_, Result<ReadResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::Read(read)).await? {
                Response::Read(answer) => Ok(answer),
                other => Err(mismatched(Method::Read, other)),
            }
        })
    }

    /// Write bytes to a file where the executor runs things; returns its size afterwards.
    fn write(&mut self, write: WriteCall) -> BoxFuture<'_, Result<WriteResp, Failure>> {
        Box::pin(async move {
            match self.call(Call::Write(write)).await? {
                Response::Write(answer) => Ok(answer),
                other => Err(mismatched(Method::Write, other)),
            }
        })
    }

    /// Take everything this session has written, as a blob a new session can start on.
    ///
    /// The result is exactly what [`InitCall::snapshot`] takes. Its contents are the
    /// executor's own encoding, readable only by an executor of the same kind; a caller
    /// just keeps the bytes and hands them over.
    ///
    /// Always one message: a truncated snapshot is a broken tree, not a smaller session,
    /// so a session that has written more than a message holds is refused.
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
    /// boot on demand, so this only moves the boot wait off the first command.
    ///
    /// `Ok` means the notification went out, not that the boot succeeded; a failed boot
    /// surfaces as [`BOOT_FAILED`](Error::BOOT_FAILED) on the next call that needs one.
    fn start(&mut self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async move { self.notify(Notification::Start).await })
    }

    /// Release what booting took (guest, mounted tree, scratch directory) while idle.
    ///
    /// The next call that needs a booted session boots again, so this costs one boot later
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
/// Unreachable against a sane peer (one call is outstanding and the transport already
/// dropped frames with other ids), so it is reported as
/// [`INTERNAL_ERROR`](Error::INTERNAL_ERROR).
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
/// Moves frames and holds no state; what a message means is the backend's.
pub trait Server: Send {
    /// The next message. `Ok(None)` is the other end closing the channel cleanly.
    ///
    /// Not cancel-safe: a dropped `recv` may have consumed part of a frame. Wait on other
    /// things between messages, not in a `select!` against this.
    fn recv(&mut self) -> BoxFuture<'_, io::Result<Option<Message>>>;

    /// Put out one response, with the id of the request it answers.
    ///
    /// Not a `Result`: a [`Response`] already covers both the
    /// answer and the [`Error`](crate::protocol::Response::Error). It names its own method,
    /// so a relaying backend hands one on without re-typing it.
    fn respond(&mut self, id: RequestId, result: Response) -> BoxFuture<'_, io::Result<()>>;
}
