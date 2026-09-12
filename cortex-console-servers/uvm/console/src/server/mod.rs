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
//! # Relaying needs no protocol knowledge, and that is the point worth reading
//!
//! Every message on this channel is a request from the client or the response to one, in
//! one order, with one outstanding at a time. So relaying them is three lines:
//! [`Guest::relay`] writes a request and reads the answer, and the loop below does that for
//! every request that arrives.
//!
//! Nothing here has a pending table or reads a meaning into anything it carries — the ids
//! are passed through untouched, so the two ends that do care are looking at the same
//! numbers. That is what a protocol with one asking end buys: a relay for it is a relay,
//! and not a client and a server at once with a pending table on each.
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
//! - **The trees.** A `file://` context is a directory on *this* host, and it is shared into
//!   the guest at that same path — see [`boot`](crate::boot). So are the artifacts and
//!   scratch trees, by the same mechanism and for the same reason. So the paths this end
//!   answers are paths the guest has, and everything after `init` relays untouched: a `cwd`
//!   the agent reports is already a name the client can open, and a `read`'s path is
//!   already one the guest can. **Nothing here translates a path**, and that is the whole
//!   reason a share lands where it does.
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

use std::path::{Path, PathBuf};

use cortex::console::{
    Call, CommitCall, CommitResp, Error, ImageSource, InitCall, InitResp, Message, NetworkAccess,
    Notification, RequestId, Response, Server, TreeMount, TreeRole, TreeSource, stdio::StdioServer,
};
use cortex_uvm_console::built::{self, LOCAL_HOST};
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
            // Which is why this logs the message and not the code: a code is for whoever
            // gets answered, and that is nobody here.
            Message::Notification(Notification::Start) => {
                if let Err(e) = session.booted().await {
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
                let answered = match session.configure(init) {
                    Ok(answer) => Response::Init(answer),
                    Err(e) => Response::Error(e),
                };
                server.respond(id, answered).await?;
            }

            // Relayed like everything else, and then finished here. The guest writes the
            // layer — only it can see the upperdir — and this end turns that into an image,
            // because only it knows what one is. The answer the client gets is this end's.
            Message::Request {
                id,
                call: Call::Commit(commit),
            } => {
                let outcome = session.commit(id, commit).await;
                server.respond(id, outcome).await?;
            }

            // Everything else is the guest's to answer, including an `exec` that carries
            // one on: what a paused execution is owed is known by the end holding it, and
            // that is not this one.
            Message::Request { id, call } => {
                let answered = match session.booted().await {
                    // Whatever the guest said, verbatim. It is already a `Response` — the
                    // frame names its own method — so relaying is handing it on rather
                    // than re-typing an answer this end did not compose.
                    Ok(guest) => match guest.relay(id, call).await {
                        Ok(answer) => answer,
                        Err(e) => {
                            // No answer is coming, and none will for anything else on this
                            // channel either. Releasing it is what makes the next call a
                            // fresh boot rather than a second failure.
                            session.release();
                            Response::Error(refused(
                                Error::INTERNAL_ERROR,
                                format!("the channel into the guest failed: {e}"),
                            ))
                        }
                    },
                    // `booted` chose the code, because only it knows which of its failures
                    // happened.
                    Err(e) => Response::Error(e),
                };
                server.respond(id, answered).await?;
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
/// The two are apart because they are wanted at different moments. What [`InitCall`] carries is
/// the session's shape and costs nothing to hold; a guest is a process, two images and a
/// couple of hundred megabytes of the host's memory, and the point of `start` and `stop`
/// being optional is that it may come and go underneath a session that does not change.
#[derive(Default)]
struct Session {
    /// What `init` said, or the defaults for a client that never sent one — which is a
    /// session that named no tree, and that is a session.
    config: InitCall,

    /// The host directory a `file://` context named, or `None` for a session with no tree.
    ///
    /// Held apart from `config` because it is the one thing in it this end acts on: the
    /// boot shares it, and the answer to `init` was spelled from it.
    context: Option<PathBuf>,

    /// Where the session leaves what it produces, and `None` for a session that named none.
    /// Shared into the guest exactly as the context is, at this same host path.
    artifacts: Option<PathBuf>,

    /// Room for the session to work in, and `None` for a session that named none. Shared as
    /// the two above are, and additionally where the session stands — see [`home`](Self::home).
    scratch: Option<PathBuf>,

    /// The base this session's guest overlays, as a reference this server has parsed, and
    /// `None` for a session that left the choice here.
    ///
    /// Held apart from `config` for the same reason `context` is: it was checked at `init` and
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
    /// is one it has not taken. Otherwise the guest goes: the tree is shared into it when it
    /// boots and the base is the disk it boots on, so a guest from before this is a guest
    /// that no longer matches the session. Dropping it is enough — the next call that needs
    /// one boots it again, and hears the `init` that has just arrived.
    ///
    /// **The path answered here is a host path**, and the guest will stand at the same one.
    /// Nothing is mounted or booted by saying so: the share happens when a guest does.
    fn configure(&mut self, config: InitCall) -> Result<InitResp, Error> {
        // All three before the guest goes, and each with the code its own member is refused
        // by: a session this backend cannot take whole is one it has not taken at all.
        let context = tree(config.context.as_ref(), TreeRole::Context)?;
        let artifacts = tree(config.artifacts.as_ref(), TreeRole::Artifacts)?;
        let scratch = tree(config.scratch.as_ref(), TreeRole::Scratch)?;
        let image = base(config.image.as_ref())?;
        let (network, host_ports) = reach(config.network.as_ref())?;
        already_built(config.image.as_ref())?;

        self.guest = None;
        self.context = context;
        self.artifacts = artifacts;
        self.scratch = scratch;
        self.image = image;
        self.network = network;
        self.host_ports = host_ports.clone();
        self.config = config;

        Ok(InitResp {
            context: self.context.as_deref().map(placed),
            artifacts: self.artifacts.as_deref().map(placed),
            scratch: self.scratch.as_deref().map(placed),
            // Where a session starts is its scratch, its context when it has no scratch, and
            // `/` when it named neither — which is what `init::prepare` puts the agent in.
            // Either way this is the same answer the agent will give when the session is
            // replayed into it, because both ends work it out the same way. See `home`.
            cwd: Some(
                self.home()
                    .map(|at| at.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".to_string()),
            ),
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

    /// Where a session with this shape starts — `None` for one that named no tree.
    ///
    /// **The scratch before the context.** A session stands somewhere before it is told
    /// anything and every relative path a command writes lands there, so standing in the
    /// context would make the tree the client gave the session the default destination for
    /// everything it produces. Worked out here *and* in the guest, by the same rule, because
    /// this end answers `init` before there is a guest to ask — see `configure`.
    fn home(&self) -> Option<&Path> {
        self.scratch.as_deref().or(self.context.as_deref())
    }

    /// Every tree this session named, with what to call each in a failure.
    fn trees(&self) -> impl Iterator<Item = (TreeRole, &Path)> {
        [
            (TreeRole::Context, self.context.as_deref()),
            (TreeRole::Artifacts, self.artifacts.as_deref()),
            (TreeRole::Scratch, self.scratch.as_deref()),
        ]
        .into_iter()
        .filter_map(|(role, path)| path.map(|path| (role, path)))
    }

    /// A guest with this session's shape, booting one if nothing has.
    ///
    /// The `init` replay is part of booting and not a step after it: an agent that has not
    /// heard one does not know which tree it is standing in, so a guest that is handed back
    /// from here is one the session has already been announced to. A replay the agent
    /// refuses is a boot that failed, for the same reason — there is nothing useful to hand
    /// back.
    ///
    /// Returns an [`Error`] and not an `anyhow` one, because two of the ways this fails are
    /// things the protocol has codes for and a client can act on. See below for why they
    /// would otherwise be lost.
    async fn booted(&mut self) -> Result<&mut Guest, Error> {
        if self.guest.is_none() {
            // Every tree has to be there before a guest is told to mount it: a directory
            // that is not on this host is the environment being wrong for a session that is
            // described correctly, which is what `MOUNT_FAILED` says — and saying it here is
            // the difference between that and a guest that comes up without a filesystem and
            // fails at the first command. The message names which of the three it was.
            for (role, at) in self.trees() {
                match std::fs::metadata(at) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => {
                        return Err(refused(
                            Error::MOUNT_FAILED,
                            format!("{role} {}: not a directory", at.display()),
                        ));
                    }
                    Err(e) => {
                        return Err(refused(
                            Error::MOUNT_FAILED,
                            format!("{role} {}: {e}", at.display()),
                        ));
                    }
                }
            }

            let image = self.image.as_ref().map(Reference::to_string);
            let mut guest = Guest::boot(
                self.context.as_deref(),
                self.artifacts.as_deref(),
                self.scratch.as_deref(),
                image.as_deref(),
                self.network,
                &self.host_ports,
                self.config.committable,
            )
            .await
            .map_err(|e| refused(Error::BOOT_FAILED, format!("booting a guest: {e}")))?;

            // The trees **do** go in the replay, and they are the same URLs the client sent:
            // the guest mounted those host directories at those host paths, so what the agent
            // is told is true on its side too. Nothing secret is in them — a `file://` URL is
            // a directory this host already has, and whatever it took to build that tree
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

impl Session {
    /// Keep what this session has written, and answer with the image it made.
    ///
    /// Two halves. The guest walks its own upperdir into the scratch it was given, because
    /// only it can see that upperdir; this end reads what it left and stitches it onto the
    /// layers the base was made of, because only it knows what an image is.
    ///
    /// A session that did not say it might commit is refused by the guest, which has neither
    /// of the two things a commit needs — and refusing there rather than here keeps one answer
    /// to the question rather than two that could disagree.
    async fn commit(&mut self, id: RequestId, commit: CommitCall) -> Response {
        let guest = match self.booted().await {
            Ok(guest) => guest,
            Err(refusal) => return Response::Error(refusal),
        };

        let wrote = match guest.relay(id, Call::Commit(commit.clone())).await {
            Ok(answer) => answer,
            Err(e) => {
                self.release();
                return Response::Error(refused(
                    Error::INTERNAL_ERROR,
                    format!("the channel into the guest failed: {e}"),
                ));
            }
        };
        // The guest's refusal is the right one — it knows whether this session was booted
        // able to commit — so it is passed through rather than reworded. Its *answer* is a
        // size and no image, which is the half it can fill in; the rest is below.
        let wrote = match wrote {
            Response::Commit(wrote) => wrote,
            other => return other,
        };

        let guest = self.guest.as_ref().expect("just relayed through it");
        let Some(scratch) = guest
            .commit
            .as_ref()
            .map(|scratch| scratch.path().to_path_buf())
        else {
            return Response::Error(refused(
                Error::INTERNAL_ERROR,
                "the guest wrote a layer and this session has nowhere it could have put it",
            ));
        };
        let base = guest.base_layers.clone();

        let made = async {
            crate::commit::keep(
                &commit,
                &scratch,
                &base,
                crate::base::store()?,
                crate::base::built_store()?,
            )
            .await
        }
        .await;

        // The image filled in on the way past, over the size the guest answered with — the
        // two halves of one response, from the two ends that each know one of them.
        match made {
            Ok(image) => Response::Commit(CommitResp {
                image: Some(image),
                ..wrote
            }),
            Err(e) => Response::Error(refused(
                Error::IO_FAILED,
                format!("keeping this session's layer: {e}"),
            )),
        }
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
fn base(asked: Option<&ImageSource>) -> Result<Option<Reference>, Error> {
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
fn reach(asked: Option<&NetworkAccess>) -> Result<(Network, Vec<u16>), Error> {
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

/// Refuse a session that named an image nobody built here.
///
/// **Said now, unlike a registry reference that cannot be resolved.** The difference is what
/// finding out costs: whether an image made here is still here is a file test, where whether a
/// registry has one is a round trip that belongs to a boot. A client told now can build the
/// thing; one told at its first command has already paid for a boot it cannot use.
fn already_built(asked: Option<&ImageSource>) -> Result<(), Error> {
    let Some(rest) = asked
        .map(|image| image.reference.as_str())
        .and_then(|reference| reference.strip_prefix(LOCAL_HOST))
    else {
        return Ok(());
    };

    // A name this host cannot even read is not an image it is missing. Told apart, because
    // the two ask the client for opposite things: "go and build it" is useless advice to
    // somebody who spelled the reference wrong, and no spelling would ever have worked.
    let id = built::digest_of(rest)
        .map_err(|e| refused(Error::INVALID_PARAMS, format!("{LOCAL_HOST}{rest}: {e}")))?;

    // And a store this server cannot open is this host's fault rather than the client's.
    let built = crate::base::built_store()
        .map_err(|e| refused(Error::IO_FAILED, format!("opening the built images: {e}")))?;

    if built.has(&id) {
        return Ok(());
    }
    Err(refused(
        Error::UNKNOWN_IMAGE,
        format!(
            "{LOCAL_HOST}{rest}: no image by that name was made here — it was never committed, \
             or this host's cache has been cleared"
        ),
    ))
}

/// The host directory one of a session's tree URLs names, or why it names none this backend
/// can use — and `None` for a tree the session did not name.
///
/// The same two refusals `cortex-local-console` makes, and deliberately the same: a client
/// cannot tell which backend answered it, so the two must not disagree about a URL they both
/// decline. Reading the URL itself is [`TreeSource`]'s and which code refuses which member is
/// [`TreeRole`]'s, which is what keeps them from drifting apart.
///
/// `file://` and nothing else, for a reason this backend has and the local one does not: a
/// tree is shared into a guest as a directory, so a kind that is not one on this host is a
/// kind there is nothing to share. What realizes an object store as a directory is a mount,
/// and mounting is the caller's.
fn tree(named: Option<&TreeSource>, role: TreeRole) -> Result<Option<PathBuf>, Error> {
    let Some(named) = named else {
        return Ok(None);
    };

    let Some(path) = named.file_path() else {
        return Err(refused(
            role.unsupported(),
            format!(
                "{role}: {}: a guest is handed a directory, so this server realizes file:// \
                 and nothing else",
                named.scheme()
            ),
        ));
    };
    if !path.is_absolute() {
        return Err(refused(
            Error::INVALID_PARAMS,
            format!("{}: a file:// {role} needs an absolute path", named.url),
        ));
    }
    Ok(Some(path.to_path_buf()))
}

/// Where a tree went, as the protocol carries it.
fn placed(at: &Path) -> TreeMount {
    TreeMount {
        path: at.to_string_lossy().into_owned(),
    }
}

/// A refusal, as the `error` a response carries instead of a result.
fn refused(code: i64, message: impl Into<String>) -> Error {
    Error::new(code, message)
}
