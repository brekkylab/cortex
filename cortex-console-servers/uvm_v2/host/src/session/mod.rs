//! Answering a console session: the wire, and what is run at the far end of it.

mod halves;
mod hostcall;
pub mod uvm;

use std::future::Future;
use std::pin::Pin;
use std::{io, path::PathBuf};

use cortex::console::stdio::StdioServer;
use cortex::console::{
    Call, Error, InitResp, Message, Notification, Response, Server, SnapshotResp, TreeMount,
    TreeSource,
};

use crate::contract::{ARTIFACTS_PATH, CONTEXT_PATH, Network};
use crate::rootfs::build;
use uvm::Mount;

pub use uvm::Uvm;

/// The reach a session gets when its client did not ask for one, and the host ports granted
/// with it.
///
/// Read from this server's own environment, because a client that says nothing about a network
/// is not asking for one setting over another — it is leaving the question to whoever started
/// this process, who is the only party here that knows what the machine it runs on is allowed
/// to talk to. A client that does have an opinion says so, and what it says wins.
const NETWORK: &str = "CORTEX_UVM_NETWORK";
const HOST_PORTS: &str = "CORTEX_UVM_HOST_PORTS";

/// Answer a console session on stdin and stdout, until the client ends it.
pub async fn run() -> anyhow::Result<()> {
    // Taken for the life of the process: from here on stdout carries frames and nothing
    // else, which is why every diagnostic goes to stderr. Nothing enforces that — it is a
    // rule, and this is where it starts applying.
    let mut server = StdioServer::stdio()?;

    // Clean up what an earlier run did not get to.
    if let Ok(entries) = std::fs::read_dir(sessions()) {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            // A pid comes round again, so one named for this process is a dead
            // predecessor's — and it is the one directory that must go, since this run is
            // about to put a session in it.
            //
            // Otherwise `kill(pid, 0)` sends no signal and only reports whether the pid could
            // be signalled: `ESRCH` — no such process — is the one answer that means the
            // directory is abandoned, where `EPERM` says the pid is alive under another user
            // and its files are not ours to remove.
            //
            // SAFETY: signal 0 performs only that permission and existence check; it
            // cannot affect this or any other process.
            let stale = pid == std::process::id() as i32
                || (unsafe { libc::kill(pid, 0) } == -1
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH));
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    // Initialized when init called.
    let mut session_factory: Option<
        Box<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<Uvm>>>>>,
    > = None;
    // Whatever it holds is dropped on `stop` and on the way out of this function whichever
    // way it leaves, which takes the machine down and deletes what it wrote every time.
    let mut session: Option<Uvm> = None;

    while let Some(message) = server.recv().await? {
        match message {
            // If already initialized
            Message::Request {
                id,
                call: Call::Init(_),
            } if session_factory.is_some() => {
                server
                    .respond(
                        id,
                        Response::Error(Error::new(
                            Error::INVALID_REQUEST,
                            "this session has already been told what it is",
                        )),
                    )
                    .await?;
            }

            // Initialize
            Message::Request {
                id,
                call: Call::Init(init),
            } => {
                let answered = match async {
                    let context = at(init.context.as_ref(), "context", Error::UNSUPPORTED_CONTEXT)?;
                    let artifacts = at(
                        init.artifacts.as_ref(),
                        "artifacts",
                        Error::UNSUPPORTED_ARTIFACTS,
                    )?;

                    // How far this session may reach, and which doors on this host it is
                    // granted. Settled before anything is built, because it is the one part of
                    // an `init` a client can be told it got wrong while the answer still costs
                    // it nothing: a reach decides whether a virtio-net device is attached, and
                    // a device is attached before a kernel comes up.
                    //
                    // Every name the protocol defines is one this backend can answer, so the
                    // only refusals are a name nobody defined and a grant that cannot mean
                    // anything.
                    let (network, host_ports) = match init.network.as_ref() {
                        // Nothing asked, so this server's own setting stands — see [`NETWORK`].
                        // That there is a setting at all, rather than no device, is what makes
                        // a name resolve: a command reaching for the internet under the default
                        // fails at a refused connection instead of at a lookup that hangs, and
                        // a session granted a host port talks to whatever the operator put
                        // there without any of it being published.
                        None => (
                            Network::parse(std::env::var(NETWORK).ok().as_deref()).map_err(
                                |_| {
                                    refuse(
                                        Error::UNSUPPORTED_NETWORK,
                                        format!(
                                            "{NETWORK}: no such reach — this server answers \
                                             none, host, public and full"
                                        ),
                                    )
                                },
                            )?,
                            // Parsed rather than passed on as the string it was written as: a
                            // boot is told ports and not a spelling of them, so a typo is this
                            // server's to report, by the name of the variable that carried it.
                            std::env::var(HOST_PORTS)
                                .unwrap_or_default()
                                .split(',')
                                .map(str::trim)
                                .filter(|port| !port.is_empty())
                                .map(|port| {
                                    port.parse::<u16>().map_err(|_| {
                                        refuse(
                                            Error::INVALID_PARAMS,
                                            format!("{HOST_PORTS}: {port} is not a port number"),
                                        )
                                    })
                                })
                                .collect::<anyhow::Result<_>>()?,
                        ),
                        Some(asked) => {
                            let network = Network::parse(Some(&asked.reach)).map_err(|_| {
                                refuse(
                                    Error::UNSUPPORTED_NETWORK,
                                    format!(
                                        "{}: no such reach — this server answers none, host, \
                                         public and full",
                                        asked.reach
                                    ),
                                )
                            })?;

                            // A door onto a machine the guest has no way to send a packet to is
                            // not a narrower session, it is two settings that cannot both have
                            // been meant. Said now, while the client can still pick which one
                            // it wanted.
                            if network == Network::Disabled && !asked.host_ports.is_empty() {
                                return Err(refuse(
                                    Error::INVALID_PARAMS,
                                    format!(
                                        "host_ports {:?} were granted to a session that asked \
                                         for no network — a port on this host needs a device to \
                                         reach it through",
                                        asked.host_ports
                                    ),
                                ));
                            }

                            (network, asked.host_ports.clone())
                        }
                    };

                    // Parse rootfs
                    let rootfs = init.rootfs.clone().ok_or_else(|| {
                        refuse(
                            Error::INVALID_PARAMS,
                            "a session here runs in a micro-VM, which boots on a rootfs, and \
                             this one named none",
                        )
                    })?;

                    // Build rootfs image
                    let image = build(rootfs).await.map_err(|e| {
                        match e.downcast_ref::<crate::rootfs::Unknown>() {
                            Some(_) => coded(Error::UNKNOWN_IMAGE, e),
                            None => coded(Error::BOOT_FAILED, e),
                        }
                    })?;

                    // Where each tree is on this side, against the one path in the guest that
                    // names it. Fixed for the session, which is why the answer below can say
                    // them before anything is mounted.
                    let mounts: Vec<Mount> = [
                        (CONTEXT_PATH, context.as_ref()),
                        (ARTIFACTS_PATH, artifacts.as_ref()),
                    ]
                    .into_iter()
                    .filter_map(|(at, from)| {
                        from.map(|from| Mount {
                            at: at.to_string(),
                            from: from.clone(),
                        })
                    })
                    .collect();

                    // Where a command starts is where the base image said, and `/` for an
                    // image that said nothing. The guest stands there before an `init` has
                    // even reached it — and creates the directory when the image named one it
                    // never made, which is what `WORKDIR` means — so this is the guest's own
                    // answer, read here because this end is the one holding the image.
                    //
                    // Never a tree the session was given. Standing in one would make it the
                    // place every relative path a command writes lands in, and the context is
                    // mounted read-only in the guest for exactly that reason.
                    //
                    // Taken before the closure below, which the image itself moves into.
                    let cwd = image.workdir.clone().unwrap_or_else(|| "/".to_string());

                    // Cloned per boot rather than borrowed, because what this closure is for
                    // is outliving the machine it makes — and the one after a `stop` has to
                    // start on the same disk the client named here.
                    let snapshot = init.snapshot;
                    let boot: Box<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<Uvm>>>>> =
                        Box::new(move || {
                            let (image, mounts, host_ports, snapshot) = (
                                image.clone(),
                                mounts.clone(),
                                host_ports.clone(),
                                snapshot.clone(),
                            );
                            Box::pin(async move {
                                Uvm::boot(
                                    &image,
                                    &mounts,
                                    network,
                                    &host_ports,
                                    snapshot.as_deref(),
                                )
                                .await
                            })
                        });
                    let uvm = boot().await.map_err(|e| coded(Error::BOOT_FAILED, e))?;

                    anyhow::Ok((
                        boot,
                        uvm,
                        InitResp {
                            context: context.as_ref().map(|_| TreeMount {
                                path: CONTEXT_PATH.to_string(),
                            }),
                            artifacts: artifacts.as_ref().map(|_| TreeMount {
                                path: ARTIFACTS_PATH.to_string(),
                            }),
                            cwd: Some(cwd),
                        },
                    ))
                }
                .await
                {
                    Ok((boot, uvm, answer)) => {
                        session_factory = Some(boot);
                        session = Some(uvm);
                        Response::Init(answer)
                    }
                    Err(e) => Response::Error(refused(&e)),
                };
                server.respond(id, answered).await?;
            }

            // Relayed like everything else and then finished here. The guest flushes what
            // the session wrote — only it can — and the disk that lands on is this end's, so
            // only this end can read the blob back out of it.
            Message::Request {
                id,
                call: Call::Snapshot(_),
            } => {
                let machine = if session.is_none() {
                    match session_factory.as_ref() {
                        Some(boot) => boot()
                            .await
                            .map_err(|e| coded(Error::BOOT_FAILED, e))
                            .map(|uvm| session.insert(uvm)),
                        // Nothing to boot, because nothing said what to boot. A call out of
                        // order and not a failure of this end's.
                        None => Err(refuse(
                            Error::INVALID_REQUEST,
                            "this session has not been told what it is",
                        )),
                    }
                } else {
                    Ok(session.as_mut().expect("just checked"))
                };
                let answered = match machine {
                    Ok(uvm) => match uvm.snapshot().await {
                        Ok(blob) => Response::Snapshot(SnapshotResp { blob }),
                        Err(e) => Response::Error(refused(&e)),
                    },
                    Err(e) => Response::Error(refused(&e)),
                };
                server.respond(id, answered).await?;
            }

            // Everything else is the guest's to answer.
            Message::Request { id, call } => {
                let machine = if session.is_none() {
                    match session_factory.as_ref() {
                        Some(boot) => boot()
                            .await
                            .map_err(|e| coded(Error::BOOT_FAILED, e))
                            .map(|uvm| session.insert(uvm)),
                        // Nothing to boot, because nothing said what to boot. A call out of
                        // order and not a failure of this end's.
                        None => Err(refuse(
                            Error::INVALID_REQUEST,
                            "this session has not been told what it is",
                        )),
                    }
                } else {
                    Ok(session.as_mut().expect("just checked"))
                };
                let answered = match machine {
                    Ok(uvm) => match uvm.call(call).await {
                        // Whatever the guest said, verbatim.
                        Ok(answer) => answer,
                        Err(e) => {
                            // No answer is coming, and none will for anything else on this
                            // channel either. Releasing it is what makes the next call a
                            // fresh boot rather than a second failure.
                            session = None;
                            Response::Error(refused(&e))
                        }
                    },
                    Err(e) => Response::Error(refused(&e)),
                };
                server.respond(id, answered).await?;
            }

            // Only ever after a `stop`, since `init` leaves a machine up: what this is worth
            // is taking one back before the call that would otherwise have paid for it.
            // Nothing answers this, so a failure is only said here — the next call that needs
            // a machine tries again and tells whoever asked for it.
            Message::Notification(Notification::Start) => {
                if session.is_none() {
                    match session_factory.as_ref() {
                        Some(boot) => match boot().await {
                            Ok(uvm) => session = Some(uvm),
                            Err(e) => eprintln!("{}: booting: {e:#}", env!("CARGO_BIN_NAME")),
                        },
                        None => eprintln!(
                            "{}: booting: this session has not been told what it is",
                            env!("CARGO_BIN_NAME")
                        ),
                    }
                }
            }

            Message::Notification(Notification::Stop) => session = None,

            // The session is over.
            Message::Notification(Notification::Quit) => break,

            // A response answers a request, and this end makes none of its own on the
            // channel it reads.
            Message::Response { id, .. } => eprintln!(
                "{}: a response arrived for request {id}, which nobody made",
                env!("CARGO_BIN_NAME")
            ),
        }
    }

    // The machine first, because what it is holding is under what goes next, and then the
    // session's own directory — the disk a `stop` was careful to leave. A session that ends
    // any way that runs this leaves nothing behind; one killed outright is the next server's
    // sweep to reclaim.
    drop(session);
    let _ = std::fs::remove_dir_all(sessions().join(std::process::id().to_string()));
    Ok(())
}

/// Where this server keeps its sessions: one directory apiece, named by the pid of the
/// process that owns it. See [`crate::home`] for what is under one.
fn sessions() -> PathBuf {
    crate::home().join("session")
}

/// A host directory a tree names, and `None` for one the session did not name.
///
/// `unsupported` is the code for the tree in `role`, which the protocol gives one of each:
/// a client refused at `init` has to know *which* of the two this server cannot take, and a
/// session names up to two.
fn at(named: Option<&TreeSource>, role: &str, unsupported: i64) -> anyhow::Result<Option<PathBuf>> {
    let Some(named) = named else {
        return Ok(None);
    };
    // A scheme and not a spelling: which kinds of tree can be realized is a fact about this
    // build, and no path written any other way would make this one of them.
    let path = named.file_path().ok_or_else(|| {
        refuse(
            unsupported,
            format!(
                "{role}: a guest is handed a directory, so this server realizes \
                 file:// and nothing else"
            ),
        )
    })?;
    // Where the scheme is one this server has, and the URL is not one it can act on. The fix
    // is the client's spelling rather than a different server, which is what parts it from the
    // refusal above.
    if !path.is_absolute() {
        return Err(refuse(
            Error::INVALID_PARAMS,
            format!(
                "{role}: a file:// tree needs an absolute path, and {} is not",
                path.display()
            ),
        ));
    }
    Ok(Some(path.to_path_buf()))
}

/// A failure that names the code the protocol has for it.
///
/// Everything under a request hands back an `anyhow::Error`, so a code travels the way a
/// message already does — carried by the failure itself and read off it where the response is
/// made — rather than by a second return type every step in between would have to thread
/// through. A failure that names none is [`INTERNAL_ERROR`](Error::INTERNAL_ERROR), which is
/// what "something went wrong at this end" means on the wire.
///
/// # Which code is which is this end's to decide, and only this end's
///
/// The store below builds images for a `rootfs build` that has no protocol at all, and the
/// machine below that boots for a build step that answers nobody. Neither has a client to say
/// a code to, so neither names one: what they hand back is the failure, and the mapping from a
/// kind of failure to a number a client branches on is made here, at the one place that is
/// talking to a client.
#[derive(Debug)]
struct Refusal {
    code: i64,
    said: anyhow::Error,
}

impl std::fmt::Display for Refusal {
    /// What was said, and not the code: the code is read off the value, and a sentence with
    /// the number in it would carry it twice wherever this is shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.said)
    }
}

impl std::error::Error for Refusal {}

/// A refusal naming `code`, said here.
fn refuse(code: i64, message: impl Into<String>) -> anyhow::Error {
    coded(code, anyhow::Error::msg(message.into()))
}

/// `said`, named as `code` — the failure kept whole and only labelled.
///
/// What a client reads is still the sentence written where the thing went wrong. A code says
/// which kind of failure this is, and nothing this end could add about it would be a better
/// sentence than the one the layer that hit it already wrote.
fn coded(code: i64, said: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(Refusal { code, said })
}

/// Anything that went wrong, as the one thing the protocol carries.
fn refused(e: &anyhow::Error) -> Error {
    // The whole chain and not the outermost failure, because a code is stated where the case
    // that has one is recognised, and context added on the way out here is what makes the
    // message readable rather than what decides which failure it is.
    let code = e
        .chain()
        .find_map(|cause| cause.downcast_ref::<Refusal>())
        .map_or(Error::INTERNAL_ERROR, |named| named.code);
    Error {
        code,
        message: format!("{e:#}"),
        data: None,
    }
}
