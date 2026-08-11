//! The server role: answer a console session by running commands inside a micro-VM.
//!
//! Almost nothing here is about running a command, and that is the design rather than an
//! omission. What runs commands is the agent inside the guest, which answers the same
//! protocol this end does — so this end is a **relay** with a guest's lifetime attached to
//! it, and the interesting code lives on the other side of a hypervisor.
//!
//! ```text
//! client ──stdio──► this process ──socket + virtio port──► guest agent
//!            ▲                ▲
//!            └ the protocol   └ the same protocol, ids and all
//! ```
//!
//! # Delegation needs no code here, and that is the point worth reading
//!
//! A delegated call is a *response*: the guest answers an `exec` with a
//! [`Delegated`](cortex::console::Progress::Delegated), and the client carries on with
//! another `exec` whose `cmd` names the request that response came on. Both directions
//! are ordinary messages on one channel, in one order, with one outstanding at a time.
//!
//! So relaying them is relaying anything. [`Guest::relay`] writes a request and reads the
//! answer, the loop below does that for every request that arrives, and a delegation chain
//! twenty calls long is twenty passes through the same three lines. Nothing here has a
//! pending table, knows what a `Delegated` is, or has to be told which execution a resume
//! belongs to — the ids are passed through untouched, so the two ends that do care are
//! looking at the same numbers.
//!
//! That property is what the protocol bought by making delegation a response instead of a
//! request from the server. A server that asked would need this relay to be a client and a
//! server at once, on both channels, with a pending table on each.
//!
//! # What this end does own
//!
//! Three things, all of them about the guest rather than about the protocol:
//!
//! - **Booting.** `exec`, `read` and `write` each need a guest and get one, which is where
//!   the cold start is paid unless a `start` got there first. On this backend that is not
//!   the formality it is on a host-local console: a boot is a kernel, an overlay and a
//!   device probe, and it is seconds rather than a `symlink(2)`.
//! - **The session's shape.** `init` is answered here and not forwarded, because a guest
//!   that does not exist yet cannot be told anything — so the session is remembered and
//!   [replayed](Session::booted) into whichever guest comes up next. A second `init`
//!   therefore releases the guest booted under the first, which is exactly what the
//!   protocol says it does.
//! - **Releasing.** `stop` drops the guest, which kills the VMM and deletes the image it
//!   was writing. `quit` ends the process, and the guest goes with it.
//!
//! # A guest that breaks takes the boot with it and not the session
//!
//! If the channel into the guest fails mid-call there is no answer and will not be one, so
//! the request is refused and the guest is released. The session stays open, and the next
//! call boots a fresh one — under the same `init`, having lost whatever the old guest's
//! image held. That is the same shape as a boot that failed in the first place, which is
//! the only shape a client already has to handle.

mod guest;

use bson::Bson;
use cortex::console::stdio::StdioServer;
use cortex::console::{Call, Error, Init, Message, Notification, Outcome, RequestId, Server};

pub use guest::BOOT_ARG;
use guest::Guest;

/// The id the replayed `init` goes out under.
///
/// It can collide with nothing: a client allocates ids from zero upwards, and there is one
/// call outstanding at a time — so this number is only ever on the wire while the relay is
/// between two of the client's requests, and it is not one a client will reach.
///
/// `i64::MAX` and not `RequestId::MAX`, because the wire is BSON and BSON has no unsigned
/// integer: an id above `i64::MAX` is a frame that will not serialize.
const REPLAYED_INIT: RequestId = i64::MAX as RequestId;

/// Answer requests until the client says `quit` or closes the channel.
pub async fn run() -> anyhow::Result<()> {
    let mut server = StdioServer::stdio()?;

    // A run that was killed outright could not clean up after itself, and what it left may
    // be a session's whole filesystem. Nobody else will.
    crate::assets::sweep_abandoned();

    // Whatever it holds is dropped on `stop` and on the way out of this function whichever
    // way it leaves, which kills the guest and deletes its image every time.
    let mut session = Session::default();

    while let Some(message) = server.recv().await? {
        match message {
            // The session is over.
            Message::Notification(Notification::Quit) => return Ok(()),

            // Booting early rather than under whichever call would have paid for it.
            // Nothing answers this, so a failure is only said here — the next call that
            // needs a guest tries again and tells whoever asked for it.
            Message::Notification(Notification::Start) => {
                if let Err(e) = session.booted().await {
                    eprintln!("{}: booting: {e}", env!("CARGO_BIN_NAME"));
                }
            }

            Message::Notification(Notification::Stop) => session.release(),

            // Answered here rather than forwarded. A session's shape outlives any one
            // guest — that is why it is `init` and not something an `exec` carries — and
            // the guest that will be told it may not exist yet.
            Message::Request {
                id,
                call: Call::Init(init),
            } => {
                session.configure(init);
                server.respond(id, Outcome::Result(Bson::Null)).await?;
            }

            // Everything else is the guest's to answer, including an `exec` that carries
            // one on: what a paused execution is owed is known by the end holding it, and
            // that is not this one.
            Message::Request { id, call } => {
                let outcome = match session.booted().await {
                    Ok(guest) => match guest.relay(id, call).await {
                        Ok(outcome) => outcome,
                        Err(e) => {
                            // No answer is coming, and none will for anything else on this
                            // channel either. Releasing it is what makes the next call a
                            // fresh boot rather than a second failure.
                            session.release();
                            refused(
                                Error::INTERNAL_ERROR,
                                format!("the channel into the guest failed: {e}"),
                            )
                        }
                    },
                    Err(e) => refused(Error::BOOT_FAILED, format!("booting a guest: {e}")),
                };
                server.respond(id, outcome).await?;
            }

            // A response answers a request, and this end makes none of its own on the
            // channel it reads.
            Message::Response { id, .. } => eprintln!(
                "{}: a response arrived for request {id}, which nobody made",
                env!("CARGO_BIN_NAME")
            ),
        }
    }
    Ok(())
}

/// What a session is here: what the client announced, and whichever guest is up.
///
/// The two are apart because they are wanted at different moments. What [`Init`] carries is
/// the session's shape and costs nothing to hold; a guest is a process, two images and a
/// couple of hundred megabytes of the host's memory, and the point of `start` and `stop`
/// being optional is that it may come and go underneath a session that does not change.
#[derive(Default)]
struct Session {
    /// What `init` said, or the defaults for a client that never sent one — which is a
    /// session with nothing delegated, and that is a session.
    config: Init,

    /// `None` until something boots one: a `start`, or the first call that needs it.
    guest: Option<Guest>,
}

impl Session {
    /// Take a new shape, and let go of any guest running under the old one.
    ///
    /// The delegated names are built into what the agent linked when it booted, so a guest
    /// from before this is a guest that no longer matches the session. Dropping it is
    /// enough — the next call that needs one boots it again, and hears the `init` that has
    /// just arrived.
    fn configure(&mut self, config: Init) {
        self.guest = None;
        self.config = config;
    }

    fn release(&mut self) {
        self.guest = None;
    }

    /// A guest with this session's shape, booting one if nothing has.
    ///
    /// The `init` replay is part of booting and not a step after it: an agent that has not
    /// heard one has no delegated names, so a guest that is handed back from here is one
    /// the session has already been announced to. A replay the agent refuses is a boot that
    /// failed, for the same reason — there is nothing useful to hand back.
    async fn booted(&mut self) -> anyhow::Result<&mut Guest> {
        if self.guest.is_none() {
            let mut guest = Guest::boot().await?;
            let outcome = guest
                .relay(REPLAYED_INIT, Call::Init(self.config.clone()))
                .await?;
            if let Some(error) = outcome.error() {
                anyhow::bail!("the guest agent refused the session: {error}");
            }
            self.guest = Some(guest);
        }
        Ok(self.guest.as_mut().expect("just booted"))
    }
}

fn refused(code: i64, message: impl Into<String>) -> Outcome {
    Outcome::Error(Error::new(code, message))
}
