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
//! - **The tree.** A `file://` workfs is a directory on *this* host, and it is shared into
//!   the guest at that same path — see [`boot`](crate::boot). So the path this end answers
//!   is the path the guest stands in, and everything after `init` relays untouched: a `cwd`
//!   the agent reports is already a name the client can open, and a `read`'s path is
//!   already one the guest can. **Nothing here translates a path**, and that is the whole
//!   reason the share lands where it does.
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

use std::path::PathBuf;

use cortex::console::{
    AbinSource, Call, Error, ImageSource, Init, InitResult, Message, NetworkAccess, Notification,
    Outcome, RequestId, Server, WorkFsMount, WorkFsSource, stdio::StdioServer,
};
use microsandbox_image::Reference;

use crate::{assets, contract::Network};
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
            //
            // Which is why this logs the message and not the `Outcome`: the code is for
            // whoever gets answered, and that is nobody here.
            Message::Notification(Notification::Start) => {
                if let Err(Outcome::Error(e)) = session.booted().await {
                    eprintln!("{}: booting: {}", env!("CARGO_BIN_NAME"), e.message);
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
                let outcome = match session.configure(init) {
                    Ok(answer) => match bson::serialize_to_bson(&answer) {
                        Ok(value) => Outcome::Result(value),
                        Err(e) => refused(Error::INTERNAL_ERROR, format!("encoding a result: {e}")),
                    },
                    Err(outcome) => outcome,
                };
                server.respond(id, outcome).await?;
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
                    // Already an answer, and already the right one — `booted` chose the
                    // code because only it knows which of its failures happened.
                    Err(outcome) => outcome,
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

    /// The host directory a `file://` workfs named, or `None` for a session with no tree.
    ///
    /// Held apart from `config` because it is the one thing in it this end acts on: the
    /// boot shares it, and the answer to `init` was spelled from it.
    workfs: Option<PathBuf>,

    /// The base this session's guest overlays, as a reference this server has parsed, and
    /// `None` for a session that left the choice here.
    ///
    /// Held apart from `config` for the same reason `workfs` is: it was checked at `init` and
    /// the boot is handed the result. **Not fetched here** — pulling an image is a boot's work,
    /// and this is only the name of one.
    image: Option<Reference>,

    /// How much of a network this session's guest gets. From the client's `init` when it said,
    /// and from this server's own environment when it did not — decided at `init` because it
    /// decides a *device*, which is attached before a kernel comes up.
    network: Network,

    /// TCP ports on this host the guest may open, on top of what `network` allows. A separate
    /// axis and not a wider reach: see [`HOST_PORTS`](cortex_uvm_boot::HOST_PORTS).
    host_ports: Vec<u16>,

    /// `None` until something boots one: a `start`, or the first call that needs it.
    guest: Option<Guest>,
}

impl Session {
    /// Take a new shape, answering what the client has to know about it.
    ///
    /// The URL is read before anything is let go of, so a session this backend cannot take
    /// is one it has not taken. Otherwise the guest goes: the delegated names are built into
    /// what the agent linked when it booted, so a guest from before this is a guest that no
    /// longer matches the session. Dropping it is enough — the next call that needs one
    /// boots it again, and hears the `init` that has just arrived.
    ///
    /// **The path answered here is a host path**, and the guest will stand at the same one.
    /// Nothing is mounted or booted by saying so: the share happens when a guest does.
    fn configure(&mut self, config: Init) -> Result<InitResult, Outcome> {
        let workfs = config.workfs.as_ref().map(directory_url).transpose()?;
        let image = base(config.image.as_ref())?;
        let (network, host_ports) = reach(config.network.as_ref())?;
        executables(&config.abin)?;

        self.guest = None;
        self.workfs = workfs;
        self.image = image;
        self.network = network;
        self.host_ports = host_ports.clone();
        self.config = config;

        let path = self
            .workfs
            .as_deref()
            .map(|path| path.to_string_lossy().into_owned());
        Ok(InitResult {
            workfs: path.clone().map(|path| WorkFsMount { path }),
            // Where a session starts is its tree, and a guest with no tree stands at `/` —
            // which is what `init::prepare` puts the agent in. Either way this is the same
            // answer the agent will give when the session is replayed into it.
            cwd: Some(path.unwrap_or_else(|| "/".to_string())),
            // The base in force, in this server's spelling of it — which is the registry a
            // bare reference turned out to name, and the one thing about it the client could
            // not have worked out. `None` is a session running on the pinned rootfs, which is
            // not an image and has no reference to give.
            image: self
                .image
                .as_ref()
                .map(|reference| ImageSource::new(reference.to_string())),
            // What the session got, whether it asked or not — the one place a client that
            // asked for nothing can learn what this server's own setting turned out to be.
            network: Some(NetworkAccess::new(network.as_str()).with_host_ports(host_ports)),
        })
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
    ///
    /// Returns an [`Outcome`] rather than an error, because two of the ways this fails are
    /// things the protocol has codes for and a client can act on. See below for why they
    /// would otherwise be lost.
    async fn booted(&mut self) -> Result<&mut Guest, Outcome> {
        if self.guest.is_none() {
            // The tree has to be there before a guest is told to mount it: a directory that
            // is not on this host is the environment being wrong for a session that is
            // described correctly, which is what `MOUNT_FAILED` says — and saying it here is
            // the difference between that and a guest that comes up without a filesystem and
            // fails at the first command.
            if let Some(workfs) = &self.workfs {
                match std::fs::metadata(workfs) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => {
                        return Err(refused(
                            Error::MOUNT_FAILED,
                            format!("{}: not a directory", workfs.display()),
                        ));
                    }
                    Err(e) => {
                        return Err(refused(
                            Error::MOUNT_FAILED,
                            format!("{}: {e}", workfs.display()),
                        ));
                    }
                }
            }

            let image = self.image.as_ref().map(Reference::to_string);
            let mut guest = Guest::boot(
                self.workfs.as_deref(),
                image.as_deref(),
                self.network,
                &self.host_ports,
                &self.config.abin,
            )
            .await
            // A name the session named twice is the one boot failure that is the client's
            // own and fixable by saying something else, so it comes back as itself rather
            // than as a boot that did not happen.
            .map_err(|e| match e.downcast_ref::<crate::abin::Duplicate>() {
                Some(duplicate) => refused(Error::DUPLICATE_EXECUTABLE, duplicate.to_string()),
                None => refused(Error::BOOT_FAILED, format!("booting a guest: {e}")),
            })?;

            // The tree **does** go in the replay, and it is the same URL the client sent:
            // the guest mounted that host directory at that host path, so what the agent is
            // told is true on its side too. Nothing secret is in it — a `file://` URL is a
            // directory this host already has, and whatever it took to build that tree
            // stayed on this side of the boundary.
            let outcome = guest
                .relay(REPLAYED_INIT, Call::Init(self.config.clone()))
                .await
                .map_err(|e| refused(Error::BOOT_FAILED, format!("announcing the session: {e}")))?;
            if let Some(error) = outcome.error() {
                return Err(refused(
                    Error::BOOT_FAILED,
                    format!("the guest agent refused the session: {error}"),
                ));
            }
            self.guest = Some(guest);
        }
        Ok(self.guest.as_mut().expect("just booted"))
    }
}

/// The base a session asked for, parsed, or `None` for one that left the choice here.
///
/// **Parsed and not fetched.** A reference that is not one is a client's mistake and is worth
/// the frame it takes to say so; a reference that resolves to nothing is a registry's answer,
/// and getting it takes a pull. Pulling at `init` would put a cold registry between a client
/// and a console that has not started doing anything yet, so it belongs to the boot that needs
/// the image — and a client that wants to pay for it early has [`Console::start`] for that.
///
/// A session that asked for nothing gets this server's own setting, which is read here so that
/// the answer to `init` can name it. **The one place the environment is consulted**: what a
/// session runs on is settled before a boot rather than discovered by one.
fn base(asked: Option<&ImageSource>) -> Result<Option<Reference>, Outcome> {
    let reference = match asked {
        Some(asked) => asked.reference.clone(),
        None => match std::env::var(assets::IMAGE_ENV) {
            Ok(reference) if !reference.is_empty() => reference,
            // Neither said, so the base is the pinned rootfs: not an image, and nothing this
            // can name.
            _ => return Ok(None),
        },
    };

    reference
        .parse()
        .map(Some)
        .map_err(|e| refused(Error::INVALID_PARAMS, format!("{reference}: {e}")))
}

/// How much of a network a session gets when its client did not say. This server's own setting,
/// read from its own environment like `CORTEX_UVM_IMAGE` and answered back at `init`.
const NETWORK: &str = "CORTEX_UVM_NETWORK";

/// The host TCP ports a session gets when its client did not say. Read here for the same reason
/// as [`NETWORK`], and parsed here because a boot is handed the ports and not the string.
const HOST_PORTS: &str = "CORTEX_UVM_HOST_PORTS";

/// The ports [`HOST_PORTS`] names, comma-separated, and an empty list for absent or empty.
fn parse_host_ports(value: Option<&str>) -> anyhow::Result<Vec<u16>> {
    value
        .unwrap_or_default()
        .split(',')
        .filter(|port| !port.is_empty())
        .map(|port| {
            port.parse::<u16>()
                .map_err(|_| anyhow::anyhow!("{HOST_PORTS}: {port} is not a port number"))
        })
        .collect()
}

/// The reach a session asked for, or this server's own when it asked for nothing — and the host
/// ports granted alongside it.
///
/// Every name the protocol defines is one this backend can answer, so the only refusals here are
/// a name nobody defined and a grant that cannot mean anything. Both are worth making at `init`
/// rather than later, because a reach decides whether a virtio-net device is attached and a
/// device is attached before a kernel comes up. A client told now can ask for something else;
/// one told at its first command has already paid for a boot it cannot use.
fn reach(asked: Option<&NetworkAccess>) -> Result<(Network, Vec<u16>), Outcome> {
    let Some(asked) = asked else {
        // Nothing asked, so this server's own setting stands — and is answered back, which is
        // how the client finds out what that was.
        let network = Network::parse(std::env::var(NETWORK).ok().as_deref())
            .map_err(|e| refused(Error::INVALID_PARAMS, e.to_string()))?;
        let ports = parse_host_ports(std::env::var(HOST_PORTS).ok().as_deref())
            .map_err(|e| refused(Error::INVALID_PARAMS, e.to_string()))?;
        return Ok((network, ports));
    };

    let network = Network::parse(Some(&asked.reach)).map_err(|_| {
        refused(
            Error::UNSUPPORTED_NETWORK,
            format!(
                "{}: no such reach — this server answers none, host, public and full",
                asked.reach
            ),
        )
    })?;

    // A door onto a machine the guest has no way to send a packet to is not a narrower session,
    // it is two settings that cannot both have been meant. Said now, while the client can pick
    // which one it wanted.
    if network == Network::Disabled && !asked.host_ports.is_empty() {
        return Err(refused(
            Error::INVALID_PARAMS,
            format!(
                "host_ports {:?} were granted to a session that asked for no network — a port on \
                 this host needs a device to reach it through",
                asked.host_ports
            ),
        ));
    }

    Ok((network, asked.host_ports.clone()))
}

/// Refuse a session whose executables are named by something this backend cannot read.
///
/// The two refusals [`directory_url`] makes about a workfs URL, and deliberately the same
/// two: both members name a host directory, and a client that had to remember which of them
/// checks what would be a client this protocol had failed.
///
/// Only the spelling is checked here. Whether two directories both carry a name is a
/// question that needs them read and cortex's own executables in hand, which is a boot's
/// work — so that one is answered on the way to a guest and comes back as
/// [`Error::DUPLICATE_EXECUTABLE`]. What this catches is the mistake a client can fix
/// without having booted anything.
fn executables(asked: &[AbinSource]) -> Result<(), Outcome> {
    for source in asked {
        let Some(path) = source.file_path() else {
            return Err(refused(
                Error::UNSUPPORTED_ABIN,
                format!(
                    "{}: this server reads `file://` and no other scheme",
                    source.url
                ),
            ));
        };
        // A relative name would be read against *this server's* working directory, which is
        // not the client's and is nothing the client can see. Refused rather than resolved:
        // where such a name does happen to exist here, the session would quietly get
        // whatever is sitting there.
        if !path.is_absolute() {
            return Err(refused(
                Error::INVALID_PARAMS,
                format!(
                    "{}: a file:// directory of executables needs an absolute path",
                    source.url
                ),
            ));
        }
    }
    Ok(())
}

/// The host directory a workfs URL names, or why it names none this backend can use.
///
/// The same two refusals `cortex-local-console` makes, and deliberately the same: a client
/// cannot tell which backend answered it, so the two must not disagree about a URL they both
/// decline. Reading the URL itself is [`WorkFsSource`]'s, which is what keeps them from
/// drifting apart.
///
/// `file://` and nothing else, for a reason this backend has and the local one does not: the
/// tree is shared into a guest as a directory, so a kind that is not one on this host is a
/// kind there is nothing to share. What realizes an object store as a directory is a mount,
/// and mounting is the caller's.
fn directory_url(workfs: &WorkFsSource) -> Result<PathBuf, Outcome> {
    let Some(path) = workfs.file_path() else {
        return Err(refused(
            Error::UNSUPPORTED_WORKFS,
            format!(
                "{}: a guest is handed a directory, so this server realizes file:// and \
                 nothing else",
                workfs.scheme()
            ),
        ));
    };
    if !path.is_absolute() {
        return Err(refused(
            Error::INVALID_PARAMS,
            format!("{}: a file:// workfs needs an absolute path", workfs.url),
        ));
    }
    Ok(path.to_path_buf())
}

fn refused(code: i64, message: impl Into<String>) -> Outcome {
    Outcome::Error(Error::new(code, message))
}
