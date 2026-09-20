//! Answering a console session: the wire, and what is run at the far end of it.

mod halves;
pub mod uvm;

use std::future::Future;
use std::pin::Pin;
use std::{io, path::PathBuf};

use cortex::console::stdio::StdioServer;
use cortex::console::{
    Call, Error, InitResp, Message, Notification, Response, Server, SnapshotResp, TreeMount,
    TreeSource,
};

use crate::contract::{ARTIFACTS_PATH, CONTEXT_PATH, Network, SCRATCH_PATH};
use crate::rootfs::build;
use uvm::Mount;

pub use uvm::Uvm;

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

    // What booting this session takes, worked out once at `init` and held as the thing that
    // does it. A boot is not the only one there will be — `stop` gives the machine back and
    // the next call that needs one takes it again — so this is an `Fn` and not an `FnOnce`,
    // and the client says what a session is exactly once however many machines answer it.
    #[allow(clippy::type_complexity)]
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
            } if session_factory.is_some() || session.is_some() => {
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
                    let context = at(init.context.as_ref(), "context")?;
                    let artifacts = at(init.artifacts.as_ref(), "artifacts")?;
                    let scratch = at(init.scratch.as_ref(), "scratch")?;

                    // What the client asked for, or nothing — and nothing means no device at
                    // all, which is the setting a sandbox should have to ask its way out of.
                    let network = match init.network.as_ref() {
                        Some(asked) => Network::parse(Some(&asked.reach))
                            .map_err(|_| anyhow::anyhow!("{}: no such reach", asked.reach))?,
                        None => Network::Disabled,
                    };

                    // The whole declaration, built: the base resolved — a digest names
                    // something already in the store, anything else is pulled — and every step
                    // over it realized. What comes back is an `Image`, which is the only thing
                    // a boot takes, and a boot is what this end has instead of a way to run a
                    // command; so a session that named no rootfs is refused here rather than
                    // taken and then refused by everything asked of it.
                    //
                    // The same call `rootfs build` makes, which is what makes the two agree:
                    // a client that built an image and a client that hands over the
                    // declaration are naming one image, and the second one pays for it once.
                    let declared = init.rootfs.clone().ok_or_else(|| {
                        anyhow::anyhow!(
                            "a session here runs in a micro-VM, which boots on a rootfs, \
                             and this one named none"
                        )
                    })?;
                    let image = build(declared).await?;

                    // Where each tree is on this side, against the one path in the guest that
                    // names it. Fixed for the session, which is why the answer below can say
                    // them before anything is mounted.
                    let mounts: Vec<Mount> = [
                        (CONTEXT_PATH, context.as_ref()),
                        (ARTIFACTS_PATH, artifacts.as_ref()),
                        (SCRATCH_PATH, scratch.as_ref()),
                    ]
                    .into_iter()
                    .filter_map(|(at, from)| {
                        from.map(|from| Mount {
                            at: at.to_string(),
                            from: from.clone(),
                        })
                    })
                    .collect();

                    // Cloned per boot rather than borrowed, because what this closure is for
                    // is outliving the machine it makes — and the one after a `stop` has to
                    // start on the same disk the client named here.
                    let snapshot = init.snapshot;
                    let boot: Box<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<Uvm>>>>> =
                        Box::new(move || {
                            let (image, mounts, snapshot) =
                                (image.clone(), mounts.clone(), snapshot.clone());
                            Box::pin(async move {
                                Uvm::boot(&image, &mounts, network, snapshot.as_deref()).await
                            })
                        });
                    let uvm = boot().await?;

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
                            scratch: scratch.as_ref().map(|_| TreeMount {
                                path: SCRATCH_PATH.to_string(),
                            }),
                            // Where a command starts is the tree it may write in freely, and
                            // the context when there is no scratch.
                            cwd: scratch
                                .as_ref()
                                .map(|_| SCRATCH_PATH.to_string())
                                .or_else(|| context.as_ref().map(|_| CONTEXT_PATH.to_string())),
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
                        Some(boot) => boot().await.map(|uvm| session.insert(uvm)),
                        None => Err(anyhow::anyhow!("this session has not been told what it is")),
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
                        Some(boot) => boot().await.map(|uvm| session.insert(uvm)),
                        None => Err(anyhow::anyhow!("this session has not been told what it is")),
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
fn at(named: Option<&TreeSource>, role: &str) -> anyhow::Result<Option<PathBuf>> {
    let Some(named) = named else {
        return Ok(None);
    };
    let path = named
        .file_path()
        .ok_or_else(|| anyhow::anyhow!("{role}: a guest is handed a directory, so this server realizes file:// and nothing else"))?;
    anyhow::ensure!(
        path.is_absolute(),
        "{role}: a file:// tree needs an absolute path, and {} is not",
        path.display()
    );
    Ok(Some(path.to_path_buf()))
}

/// Anything that went wrong, as the one thing the protocol carries.
fn refused(e: &anyhow::Error) -> Error {
    Error {
        code: Error::INTERNAL_ERROR,
        message: format!("{e:#}"),
        data: None,
    }
}
