//! Answering a console session: the wire, and what is run at the far end of it.

#[cfg(target_os = "macos")]
mod entitlement;
pub mod uvm;

pub use uvm::boot;

use std::{io, path::PathBuf};

use cortex::console::stdio::StdioServer;
use cortex::console::{
    Call, CommitResp, Error, ImageSource, InitCall, InitResp, Message, Notification, Response,
    Server, TreeMount, TreeSource,
};

use crate::contract::{ARTIFACTS_PATH, CONTEXT_PATH, Network, SCRATCH_PATH};
use crate::rootfs::{Image, pull};
use uvm::Mount;

pub use uvm::Uvm;

/// Marks a path on the host as this server's, and is where the owning pid starts.
///
/// The shape the sweep below reads back is `{PREFIX}{pid}-{seq}-{what}`: the pid says who owns
/// the path, and the counter keeps two boots of one server apart — a `stop` and the next
/// `start` are two images, and the first is still being deleted while the second is being
/// formatted. Whatever ends up creating these paths has to spell them that way, or the sweep
/// leaks exactly the files it exists to reclaim.
///
/// Deliberately not `cortex-uvm-`, which is the first micro-VM console server's: the two can
/// be running at once, and each should only ever reclaim its own. The prefixes do not collide
/// in either direction — that one's sweep strips `cortex-uvm-` from these names and finds
/// `v2` where it wants a pid, so it skips them, and these names are the only ones this prefix
/// matches.
const PREFIX: &str = "cortex-uvm-v2-";

/// Answer a console session on stdin and stdout, until the client ends it.
pub async fn run() -> anyhow::Result<()> {
    // Taken for the life of the process: from here on stdout carries frames and nothing
    // else, which is why every diagnostic goes to stderr. Nothing enforces that — it is a
    // rule, and this is where it starts applying.
    let mut server = StdioServer::stdio()?;

    // Then delete what a server that was killed outright left behind.
    //
    // A session's own values remove their files when they drop, which covers a session
    // ending any way that runs a destructor — a `stop`, a `quit`, a process exiting on
    // its own. What no destructor covers is `SIGKILL`, and what is lost to it is a sparse
    // image that may hold everything a session wrote.
    //
    // So this sweep is the only thing that ever reclaims those, and it runs at the start
    // of every server rather than at the end of any: a process that was killed is not one
    // that gets to clean up, and the next one is the first thing that can.
    //
    // Best-effort throughout, because none of it is this session's to succeed at: a path
    // that cannot be removed is left for the next run, which is where it already was.
    //
    // Both directories, because the two differ on macOS: a socket has to live under
    // `/tmp` for its path to fit in `sockaddr_un`, where everything else uses the
    // per-user temp directory.
    let mut dirs = vec![std::env::temp_dir()];
    if !dirs.contains(&PathBuf::from("/tmp")) {
        dirs.push(PathBuf::from("/tmp"));
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|n| n.strip_prefix(PREFIX))
                .and_then(|rest| rest.split('-').next())
                .and_then(|pid| pid.parse::<i32>().ok())
            else {
                continue;
            };
            // `kill(pid, 0)` sends no signal and only reports whether the pid could be
            // signalled: `ESRCH` — no such process — is the one answer that means the path
            // is abandoned, where `EPERM` says the pid is alive under another user and its
            // files are not ours to remove.
            //
            // SAFETY: signal 0 performs only that permission and existence check; it
            // cannot affect this or any other process.
            let gone = unsafe { libc::kill(pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if !gone {
                continue;
            }
            let path = entry.path();
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }

    // Whatever it holds is dropped on `stop` and on the way out of this function whichever
    // way it leaves, which takes the machine down and deletes what it wrote every time.
    let mut session = Session::default();

    while let Some(message) = server.recv().await? {
        match message {
            // The session is over.
            Message::Notification(Notification::Quit) => return Ok(()),

            // Booting early rather than under whichever call would have paid for it. Nothing
            // answers this, so a failure is only said here — the next call that needs a
            // machine tries again and tells whoever asked for it.
            Message::Notification(Notification::Start) => {
                if let Err(e) = session.booted().await {
                    eprintln!("{}: booting: {e}", env!("CARGO_BIN_NAME"));
                }
            }

            Message::Notification(Notification::Stop) => session.uvm = None,

            // Answered here rather than forwarded. A session's shape outlives any one
            // machine — that is why it is `init` and not something an `exec` carries — and
            // the machine that will be told it may not exist yet.
            Message::Request {
                id,
                call: Call::Init(init),
            } => {
                let answered = match session.configure(init).await {
                    Ok(answer) => Response::Init(answer),
                    Err(e) => Response::Error(refused(&e)),
                };
                server.respond(id, answered).await?;
            }

            // Relayed like everything else and then finished here. The guest writes the
            // layer — only it can see the upperdir — and this end turns that into an image,
            // because only it knows what one is.
            Message::Request {
                id,
                call: Call::Commit(commit),
            } => {
                let answered = match session.commit(commit.env, commit.working_dir).await {
                    Ok(named) => Response::Commit(CommitResp {
                        size: 0,
                        image: Some(ImageSource::new(named)),
                    }),
                    Err(e) => Response::Error(refused(&e)),
                };
                server.respond(id, answered).await?;
            }

            // Everything else is the guest's to answer.
            Message::Request { id, call } => {
                let answered = match session.booted().await {
                    Ok(uvm) => match uvm.call(call).await {
                        // Whatever the guest said, verbatim.
                        Ok(answer) => answer,
                        Err(e) => {
                            // No answer is coming, and none will for anything else on this
                            // channel either. Releasing it is what makes the next call a
                            // fresh boot rather than a second failure.
                            session.uvm = None;
                            Response::Error(refused(&e))
                        }
                    },
                    Err(e) => Response::Error(refused(&e)),
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

/// What a session is here: what the client announced, and whichever machine is up.
///
/// The two are apart because they are wanted at different moments. What `init` carries costs
/// nothing to hold; a machine is a process, two disks and a couple of hundred megabytes, and
/// the point of `start` and `stop` being optional is that it may come and go underneath a
/// session that does not change.
#[derive(Default)]
struct Session {
    context: Option<PathBuf>,
    artifacts: Option<PathBuf>,
    scratch: Option<PathBuf>,
    /// What the session boots on, resolved once at `init`.
    image: Option<Image>,
    /// How much of the network a command may reach.
    network: Network,
    uvm: Option<Uvm>,
}

impl Session {
    /// Take what the client announced, and say where each tree landed.
    ///
    /// Answered without a machine: the shape is the session's and the machine that will be
    /// told it may not exist yet. A second `init` replaces the first and releases whatever
    /// was booted on the old one.
    async fn configure(&mut self, init: InitCall) -> anyhow::Result<InitResp> {
        self.uvm = None;
        self.context = at(init.context.as_ref(), "context")?;
        self.artifacts = at(init.artifacts.as_ref(), "artifacts")?;
        self.scratch = at(init.scratch.as_ref(), "scratch")?;

        // A digest names something already in the store; anything else is an OCI reference
        // and is pulled. Both end as an `Image`, which is the only thing a boot takes.
        // What the client asked for, or nothing — and nothing means no device at all, which
        // is the setting a sandbox should have to ask its way out of.
        self.network = match init.network.as_ref() {
            Some(asked) => Network::parse(Some(&asked.reach))
                .map_err(|_| anyhow::anyhow!("{}: no such reach", asked.reach))?,
            None => Network::Disabled,
        };

        self.image = match init.image.as_ref().map(|named| named.reference.as_str()) {
            None => None,
            Some(named) if named.starts_with("sha256:") => Some(Image::load(&named.parse()?)?),
            Some(named) => Some(pull(named).await?),
        };

        Ok(InitResp {
            context: self.context.as_ref().map(|_| TreeMount { path: CONTEXT_PATH.to_string() }),
            artifacts: self.artifacts.as_ref().map(|_| TreeMount { path: ARTIFACTS_PATH.to_string() }),
            scratch: self.scratch.as_ref().map(|_| TreeMount { path: SCRATCH_PATH.to_string() }),
            // Where a command starts is the tree it may write in freely, and the context
            // when there is no scratch.
            cwd: self
                .scratch
                .as_ref()
                .map(|_| SCRATCH_PATH.to_string())
                .or_else(|| self.context.as_ref().map(|_| CONTEXT_PATH.to_string())),
            image: self
                .image
                .as_ref()
                .and_then(|image| image.digest().ok())
                .map(|digest| ImageSource::new(digest.to_string())),
            network: init.network,
        })
    }

    /// The machine this session is running on, booted if it is not up.
    async fn booted(&mut self) -> anyhow::Result<&mut Uvm> {
        if self.uvm.is_none() {
            let image = self
                .image
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("this session named no image to boot"))?;

            let mut mounts = Vec::new();
            for (at, from) in [
                (CONTEXT_PATH, self.context.as_ref()),
                (ARTIFACTS_PATH, self.artifacts.as_ref()),
                (SCRATCH_PATH, self.scratch.as_ref()),
            ] {
                if let Some(from) = from {
                    mounts.push(Mount {
                        at: at.to_string(),
                        from: from.clone(),
                    });
                }
            }
            self.uvm = Some(Uvm::boot(image, &mounts, self.network).await?);
        }
        Ok(self.uvm.as_mut().expect("just booted"))
    }

    /// What the session wrote, as an image of its own.
    ///
    /// The layer is the guest's and the image is this end's: the two halves of a commit, and
    /// neither knows what the other holds.
    async fn commit(
        &mut self,
        env: Vec<String>,
        working_dir: Option<String>,
    ) -> anyhow::Result<String> {
        let layer = self.booted().await?.commit().await?;

        let mut image = self
            .image
            .clone()
            .ok_or_else(|| anyhow::anyhow!("this session named no image to commit over"))?;
        image.layers.push(layer);
        image.env = env;
        image.workdir = working_dir;

        Ok(image.store()?.to_string())
    }
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
