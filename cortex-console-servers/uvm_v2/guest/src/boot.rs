//! `boot` mode: answer a console session by running commands in this guest.
//!
//! What arrives here has come a long way — a client's `exec`, framed on the host, handed
//! to a virtio-console port, read out of a character device in here — and none of that is
//! visible below. A [`Server`] over the port brings requests in and puts responses out,
//! and what is left is what is actually this end's: running a command, keeping track of
//! where the session stands, and reading and writing the files a command works with.
//!
//! Which is the same list [`cortex-local-console`]'s server has, answered the same way.
//! That is worth stating rather than hiding: the two backends differ in *where* a command
//! runs and in nothing else, and the code below being recognisable is the shape of that
//! fact rather than an accident. See [What is duplicated](#what-is-duplicated).
//!
//! # The channel is a virtio port, not stdin and stdout
//!
//! Stdin and stdout in here belong to the guest's console, which is where the kernel
//! prints and where a command's own output would go if anything let it through. Putting
//! frames on that would mean a protocol stream a single kernel warning could corrupt.
//!
//! So the session runs over a named virtio-console port —
//! [`PORT_NAME`](crate::contract::PORT_NAME) — opened by [`init`](crate::init) before
//! this starts. Two halves from one descriptor, because the driver allows a single opener
//! per port: [`run`] `dup`s the handle rather than opening the device twice.
//!
//! The first thing on it is [`HANDSHAKE`](crate::contract::HANDSHAKE), sent by this end.
//! It is not part of the protocol and it is not politeness — a virtio-console port
//! discards what the host writes while no process in the guest has it open, so the host
//! has to be told when reading has started, and there is nothing in the protocol for this
//! end to say first.
//!
//! # Running the command
//!
//! [`Command`], captured: spawned, both pipes drained, and one response when it ends. An
//! execution is one request and one answer, so there is nothing to interleave and nothing
//! to hold — which is what makes [`execute`] a function of the request rather than
//! something threaded through the channel.
//!
//! # Where the session stands
//!
//! It starts at the tree the boot shared, and moves only through
//! [`cd`](Session::change_dir), which this agent answers itself: `cd` is a shell builtin
//! rather than a program, so an `exec` carrying an argv has no other way to offer it, and
//! the shell a person would be talking to is the session. A `cd` inside a command
//! (`sh -c 'cd x'`) moves that command's own process and nothing else, exactly as a
//! subshell does in a terminal.
//!
//! Nothing is reported about it either way. A `cd` is answered with a code and no output,
//! the way a terminal answers one, and a client that wants to know where it is runs `pwd`
//! — which lands here as an ordinary command, in the directory this session is holding.
//!
//! # Files
//!
//! `read` and `write` reach this guest's filesystem directly, at the path as given — the
//! same namespace a command here resolves its own relative paths in, since a command here
//! is an ordinary process of this one's, started in the same working directory.
//!
//! Which is the whole reason the protocol makes every call boot first. On the host-local
//! backend that rule buys nothing, because the filesystem is there either way; here there
//! is no file to name until a guest exists to hold it.
//!
//! # What is duplicated
//!
//! [`execute`], the `cd` builtin and the two file calls are what a host-local console
//! does, in a guest. They are written out again here rather than shared, and that is a
//! debt with a known payment: the pieces belong in cortex's console module, as helpers a
//! backend calls, so that a backend implements only what actually differs — where a
//! command runs, what booting means, what has to be released. Until then, a fix to one of
//! them is a fix owed to the other.
//!
//! # What this does not do
//!
//! A `read` or a `write` is not confined to anywhere. The path is used as it arrives, so
//! a client can name any file in the guest — which is a smaller claim than it sounds,
//! since the guest is one session's overlay and the client is the one who asked for it.
//!
//! `exec.timeout_ms` is enforced by a kill; `init` carries no default. An `exec` that
//! names no timeout is therefore unbounded — a command that never ends is one this agent
//! waits on forever, and the client waits with it.
//!
//! What a timed-out command started is killed with it, and this agent is the guest's
//! PID 1: whatever those processes were parented to, their corpses come here, and nothing
//! here reaps a child it did not spawn — a general `waitpid(-1)` would take statuses tokio
//! is waiting on. A guest that times out many commands is a guest with that many zombie
//! entries, which a session's lifetime bounds.
//!
//! [`cortex-local-console`]: https://docs.rs/cortex-local-console

use std::ffi::OsString;
use std::io::{self, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::time::Duration;

use cortex::console::stdio::StdioServer;
use cortex::console::{
    Call, Error, ExecCall, ExecResp, InitCall, InitResp, MAX_PAYLOAD, Message, Notification,
    ReadCall, ReadResp, Response, Server, SnapshotResp, TreeMount, TreeRole, TreeSource, WriteCall,
    WriteResp,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::process::{Child, Command};

use crate::contract::{
    ABIN_PATH, COMMIT_ENV, COMMIT_PATH, GUEST_BIN_PATH, GUEST_PATH, HANDSHAKE, ImageSpec,
    LAYER_TAR, UPPER_DIR,
};

/// A command we found but could not start, and one we could not find at all.
///
/// Both are codes an ordinary command can reach by its own `exit()`, which is why neither
/// goes back *as* a code: they travel as an [`Error`], which cannot be mistaken for
/// something a command chose.
const NOT_EXECUTABLE: i32 = 126;
const NOT_FOUND: i32 = 127;

/// How much of a file one response may carry.
///
/// A frame is capped at [`MAX_PAYLOAD`], and a `read`'s answer is its `data` plus the
/// members around it — `jsonrpc`, `id`, `result`, and the `size` beside the bytes. A
/// kibibyte covers those several times over.
const MAX_DATA: u64 = MAX_PAYLOAD as u64 - 1024;

/// How much of one of a command's two streams a response may carry.
///
/// Half of [`MAX_DATA`] apiece, because an [`ExecResp`] carries both and either one can be
/// the large one. What is over it is dropped and [`ExecResp::truncated`] says so — see
/// [`finished`].
const MAX_STREAM: usize = MAX_DATA as usize / 2;

/// Answer requests on `port` until the client says `quit` or the channel closes.
///
/// `port` is the virtio-console port, open read-write. It becomes two halves here, from
/// one `dup`: the driver refuses a second opener, and the framing wants to read and write
/// through borrows that do not have to take turns.
///
/// `image` is what the base image stated, read by [`init`](crate::init) while its file was
/// still reachable. It is the session's for the whole process because it describes the root
/// every session in here runs on, which no `init` can change.
/// Answer the host on the port it is holding, until the session ends.
///
/// The agent waits on things it does not drive — a port, a command — so it is async. The
/// runtime is built by hand rather than by `#[tokio::main]` because the init above it is not:
/// `pivot_root` moves the root out from under every thread in the process, and that is easier
/// to reason about when there is only one.
pub fn run(image: crate::contract::ImageSpec) -> anyhow::Result<()> {
    let port = crate::init::open_port()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(port, image))
}

async fn serve(port: std::fs::File, image: ImageSpec) -> anyhow::Result<()> {
    // SAFETY: `dup` hands back a fresh descriptor for the same open file, and the
    // `File` built from it is the only owner of that number.
    let duplicate = unsafe { libc::dup(port.as_raw_fd()) };
    if duplicate < 0 {
        anyhow::bail!(
            "duplicating the console port: {}",
            io::Error::last_os_error()
        );
    }
    let mut outgoing = unsafe { std::fs::File::from_raw_fd(duplicate) };

    // Before anything else on the wire, and before the framing owns the descriptor: the
    // host discards what it writes to a port nothing in here has opened, so this is what
    // says a reader exists. Eight bytes onto a queue nothing has filled yet, so the write
    // being the blocking one is a distinction without a wait.
    greet(&mut outgoing)?;

    // Blocking descriptors driven through tokio's blocking pool. A virtio port is
    // pollable and an `AsyncFd` would be the tighter fit, but this channel is read at
    // exactly one place and written at exactly one other — a thread parked on the next
    // request is what waiting for the next request looks like either way.
    let mut server = StdioServer::new(
        tokio::fs::File::from_std(port),
        tokio::fs::File::from_std(outgoing),
    );

    let mut session = Session::new(image);

    while let Some(message) = server.recv().await? {
        match message {
            // The session is over. The host stops the guest right after this, so there is
            // nothing to release that the VM going away does not release.
            Message::Notification(Notification::Quit) => return Ok(()),

            // Booting early rather than under whichever call would have paid for it.
            // Nothing answers this, so a failure is only said here — the next call that
            // needs a boot tries again and tells whoever asked for it.
            Message::Notification(Notification::Start) => {
                if let Err(e) = session.boot() {
                    eprintln!(
                        "{}: booting a session: {}",
                        env!("CARGO_BIN_NAME"),
                        e.message
                    );
                }
            }

            Message::Notification(Notification::Stop) => session.release(),

            Message::Request { id, call } => match call {
                // A session is taken or it is not: a tree this agent cannot stand in is
                // refused rather than answered with a path, since a path is what every
                // later call is spelled in. See [`Session::configure`].
                Call::Init(init) => {
                    let answered = match session.configure(init) {
                        Ok(answer) => Response::Init(answer),
                        Err(e) => Response::Error(e),
                    };
                    server.respond(id, answered).await?;
                }

                Call::Exec(exec) => match session.boot() {
                    Err(e) => server.respond(id, Response::Error(e)).await?,
                    // `cd` is the session's own and not a program: see
                    // [`Session::change_dir`]. It is answered here rather than spawned,
                    // and it is the one thing that moves where the session stands.
                    Ok(()) => match cd_target(&exec) {
                        Some(argv) => {
                            let answered = session.change_dir(argv);
                            server.respond(id, answered).await?;
                        }
                        None => {
                            let answered = execute(&exec, &session).await;
                            server.respond(id, answered).await?
                        }
                    },
                },

                Call::Read(read) => {
                    let answered = match session.boot() {
                        Ok(()) => read_file(session.cwd(), &read).await,
                        Err(e) => Response::Error(e),
                    };
                    server.respond(id, answered).await?;
                }

                Call::Write(write) => {
                    let answered = match session.boot() {
                        Ok(()) => write_file(session.cwd(), &write).await,
                        Err(e) => Response::Error(e),
                    };
                    server.respond(id, answered).await?;
                }

                // Answered with an empty blob, because the snapshot is a disk this end
                // cannot read: the session's writes live on `/dev/vda`, and the file behind
                // that device is the host's. What is this end's is getting them *onto* it —
                // see [`SnapshotPlan`].
                Call::Snapshot(_) => {
                    // On a blocking thread: a sync waits on every dirty page the session
                    // wrote, and archiving an upperdir is as big as what it holds. Either
                    // one here would stop this task reading the channel — so an `exec`
                    // already in flight could not be answered and a `stop` could not even
                    // arrive.
                    let outcome = match session.boot() {
                        Ok(()) => {
                            let plan = snapshot_plan(&session);
                            tokio::task::spawn_blocking(move || plan.run())
                                .await
                                .unwrap_or_else(|e| {
                                    Response::Error(refused(
                                        Error::IO_FAILED,
                                        format!("flushing this session's writes: {e}"),
                                    ))
                                })
                        }
                        Err(refusal) => Response::Error(refusal),
                    };
                    server.respond(id, outcome).await?;
                }
            },

            // A response answers a request, and this end makes none.
            Message::Response { id, .. } => eprintln!(
                "{}: a response arrived for request {id}, which nobody made",
                env!("CARGO_BIN_NAME")
            ),
        }
    }
    Ok(())
}

/// Put [`HANDSHAKE`] on the channel, through the same descriptor the frames will use.
///
/// Flushed on the way out, because what comes next is this end waiting for a request the
/// host will not send until these bytes arrive.
fn greet(outgoing: &mut std::fs::File) -> io::Result<()> {
    use std::io::Write as _;
    outgoing.write_all(HANDSHAKE)?;
    outgoing.flush()
}

/// What a session is in here: the trees it was given, where it stands, the root it runs on,
/// and whether it has booted.
///
/// The [`InitCall`] that produced it is not kept: everything in it that this end acts on is
/// the two trees, and the base and the reach are answered one boot up by the server that
/// started this guest — see [`configure`](Self::configure).
///
/// **The same shape the host-local backend has**, deliberately: a client cannot tell which
/// one answered it, so what a session *is* must not depend on which one did. What differs
/// between them is only where a command runs, which is the whole of what a backend is for.
struct Session {
    /// The tree the session works in, as `init` named it — and as it is mounted in here.
    ///
    /// One string, because the `init` this end hears is the console server's replay and
    /// names the tree by the guest's own mount point rather than by the host directory
    /// behind it. So what the client was told, what that replay says, and what this end
    /// opens are all `/context`. See `crate::init::share`.
    context: Option<PathBuf>,

    /// Where the session leaves what it produces, mounted in here on the same terms.
    ///
    /// Nothing in this agent treats it differently from the context: both are directories a
    /// command reaches by path, and what the tree is *for* is the client's. Held so that a
    /// boot can check it is there, which is the whole of this end's part in it.
    artifacts: Option<PathBuf>,

    /// Where the session stands, which is what an execution runs in and what a relative
    /// path in a file call resolves against.
    ///
    /// Never absent: a process stands somewhere whatever it was told, and [`home`] is where
    /// this one starts. An `Option` here would have put a state that cannot happen into
    /// every call that resolves a path.
    cwd: PathBuf,

    /// What the base image stated. A property of the root rather than of the session, so an
    /// `init` neither sets it nor clears it.
    image: ImageSpec,

    /// Whether the tree `init` named has been found where the boot put it.
    ///
    /// A flag and not a resource, because the mount is the *boot's* — `init::prepare`
    /// mounted the share before this process started, from what libkrun was told — so what
    /// happens here is the check and not the mounting. What the flag earns is that the
    /// check happens once per boot rather than once per request, and that `stop` means the
    /// same thing here as on a backend where it hands something back.
    booted: bool,
}

impl Session {
    /// A session on the root the image describes, standing where that image said, with no
    /// tree configured yet.
    fn new(image: ImageSpec) -> Session {
        Session {
            cwd: home(&image),
            image,
            context: None,
            artifacts: None,
            booted: false,
        }
    }

    /// Take a new shape, answering what the client has to know about it.
    ///
    /// Every URL is read before anything is let go of, so a session this agent cannot take is
    /// one it has not taken. Otherwise the old boot goes: the trees are what a boot checked
    /// for, and a boot from before this is a boot that no longer matches.
    fn configure(&mut self, config: InitCall) -> Result<InitResp, Error> {
        let context = tree(config.context.as_ref(), TreeRole::Context)?;
        let artifacts = tree(config.artifacts.as_ref(), TreeRole::Artifacts)?;

        self.release();
        self.context = context;
        self.artifacts = artifacts;
        // Back to where the image said, because an `init` is the session starting over and a
        // `cd` from the session before it is not something the new one asked for. The root is
        // the same root, so this is the same answer [`init::prepare`] already stood in.
        self.cwd = home(&self.image);

        Ok(InitResp {
            context: self.context.as_deref().map(placed),
            artifacts: self.artifacts.as_deref().map(placed),
            cwd: self.named_cwd(),
        })
    }

    /// Where the session stands.
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Every tree this session named, with what to call each in a failure.
    fn trees(&self) -> impl Iterator<Item = (TreeRole, &Path)> {
        [
            (TreeRole::Context, self.context.as_deref()),
            (TreeRole::Artifacts, self.artifacts.as_deref()),
        ]
        .into_iter()
        .filter_map(|(role, path)| path.map(|path| (role, path)))
    }

    /// The same, as the protocol can carry it — `None` for a directory with no string form,
    /// which is a path the client could not have been told about anyway.
    fn named_cwd(&self) -> Option<String> {
        self.cwd().to_str().map(str::to_owned)
    }

    /// Forget that the tree was checked for.
    ///
    /// Which is the whole of what a `stop` gives back in here — the share is the boot's and
    /// stays mounted — so this is not the release it is on the host side, where a whole
    /// guest goes away. It is still not a no-op: the next call that needs a session checks
    /// again, which is what a client that sent `stop` is asking for.
    fn release(&mut self) {
        self.booted = false;
    }

    /// Bring the session up if nothing has: the tree where `init` said it would be.
    ///
    /// The tree was mounted before this process started — `init::prepare` did it, from what
    /// the boot named — so realizing it here is checking that it is there, which is what
    /// [`MOUNT_FAILED`](Error::MOUNT_FAILED) reports when it is not. A guest whose share
    /// failed to mount is a session described correctly in an environment that is wrong for
    /// it, which is exactly that code's case.
    fn boot(&mut self) -> Result<(), Error> {
        if self.booted {
            return Ok(());
        }

        // Every tree the session named, because the boot shared every one of them and a
        // client will send paths into both. The message says which: one code, two places it
        // can be about.
        for (role, at) in self.trees() {
            match std::fs::metadata(at) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    return Err(refused(
                        Error::MOUNT_FAILED,
                        format!("{role} {}: not a directory in the guest", at.display()),
                    ));
                }
                Err(e) => {
                    return Err(refused(
                        Error::MOUNT_FAILED,
                        format!("{role} {}: not mounted in the guest: {e}", at.display()),
                    ));
                }
            }
        }

        self.booted = true;
        Ok(())
    }

    /// Move where the session stands.
    ///
    /// **`cd` is the session's own because it is nobody else's.** It is a shell builtin and
    /// not a program, so an `exec` carrying an argv has no other way to offer it — and the
    /// shell a person would be talking to is, here, the session itself. A `cd` inside a
    /// command (`sh -c 'cd x'`) moves that command's own process and nothing else, which is
    /// what a subshell does in a terminal too.
    ///
    /// The answer is a result and not an error, including when it fails: `cd` behaves like
    /// the builtin it stands in for.
    ///
    /// **Logical, like a shell's own.** The path is normalized on paper and symlinks are
    /// left alone, so what goes back is spelled the way `init` answered — which here is also
    /// the way the *host* spells it, since the tree is mounted at the host's own path.
    fn change_dir(&mut self, argv: &[String]) -> Response {
        let target = match argv {
            [] => home(&self.image),
            [dir] => at(self.cwd(), dir),
            _ => return builtin_failed("cd: too many arguments"),
        };

        let moved = lexical(&target);
        match std::fs::metadata(&moved) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return builtin_failed(format!("cd: {}: not a directory", moved.display())),
            Err(e) => return builtin_failed(format!("cd: {}: {e}", moved.display())),
        }
        if moved.to_str().is_none() {
            return builtin_failed(format!(
                "cd: {}: has no name this protocol can carry",
                moved.display()
            ));
        }

        self.cwd = moved;
        // Nothing on the result says where that was. A shell answers `cd` with nothing
        // either, and a client that wants to know runs `pwd` — see
        // [`ExecResp`](cortex::console::ExecResp).
        result(ExecResp {
            code: 0,
            ..ExecResp::default()
        })
    }
}

/// The argv of an `exec` that is a `cd`, without the name — or `None` for anything else.
fn cd_target(exec: &ExecCall) -> Option<&[String]> {
    match exec.split() {
        Some((program, args)) if program == "cd" => Some(args),
        _ => None,
    }
}

/// The directory one of a session's tree URLs names, or why it names none this agent can use
/// — and `None` for a tree the session did not name.
///
/// `file://` and nothing else. Inside a guest that is not a limitation the way it is on the
/// host: whatever a tree is made of was realized before the VM started, and what reaches here
/// is the directory it was mounted at.
///
/// One function for both, which is the same arrangement the host-local backend has and for
/// the same reason: the trees differ in what the client means by them and not in how a URL is
/// read. `role` is what makes a refusal name which of them it was about.
fn tree(named: Option<&TreeSource>, role: TreeRole) -> Result<Option<PathBuf>, Error> {
    let Some(named) = named else {
        return Ok(None);
    };

    let Some(path) = named.file_path() else {
        return Err(refused(
            role.unsupported(),
            format!(
                "{role}: {}: a guest is handed a mounted directory, so file:// is the only \
                 kind it can be told about",
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

/// `path` with `.` dropped and `..` popped, on paper and without touching the filesystem.
///
/// What a shell does to keep a working directory readable, and here it also keeps it
/// comparable: every path this end reports has to be spelled under the one `init` answered
/// with, and resolving symlinks would produce a second spelling the client cannot relate to
/// the first.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            component => out.push(component),
        }
    }
    out
}

/// The file a path names, resolved where the session stands.
///
/// An absolute path is already the answer and `Path::join` says so by dropping the root it
/// was given; a relative one lands where a command would have looked for it. **A join, not
/// containment** — what it buys is that the two halves of the protocol name the same file.
fn at(root: &Path, path: impl AsRef<Path>) -> PathBuf {
    root.join(path)
}

/// Where a session on this image starts, and what `cd` with no argument goes back to.
///
/// **What the image stated**, which is what `init::prepare` stood this process in before any
/// `init` arrived — so the two agree without either asking the other.
///
/// `/` for an image that said nothing, and for a directory that is not there: `init::prepare`
/// creates the one the image named, the way `WORKDIR` does, so a name that is still not a
/// directory by now is one this guest could not make — and answering it anyway would be a
/// session whose every `exec` fails on a directory it was told it was standing in.
fn home(image: &ImageSpec) -> PathBuf {
    match image.working_dir.as_deref().map(PathBuf::from) {
        Some(stated) if stated.is_dir() => stated,
        _ => PathBuf::from("/"),
    }
}

/// A builtin that ran and failed, the way the shell builtin it stands in for would: a code
/// and a line, and not a refusal of the call.
fn builtin_failed(message: impl Into<String>) -> Response {
    let mut stderr: Vec<u8> = message.into().into_bytes();
    stderr.push(b'\n');
    result(ExecResp {
        code: 1,
        stderr,
        ..ExecResp::default()
    })
}

/// Run one command, and answer with everything it produced.
///
/// One request, one answer: the command is spawned, both of its pipes are drained, and
/// what comes back is how it ended.
///
/// `timeout_ms` is a kill, as the protocol promises: the command runs in a process group
/// of its own so that what a shell spawned goes with it, and expiry answers
/// [`TIMED_OUT`](Error::TIMED_OUT) with nothing of the partial output — a killed command
/// has no result to report. No timeout is no limit, and the wait is the whole command's.
///
/// # Where it runs
///
/// Where the session stands, set per command rather than inherited. A session moves — `cd`
/// is answered by [`Session::change_dir`] and the next command runs where it left off — so
/// this process's own working directory would be a second answer to that question, right
/// only until the first `cd`. `read_file` and `write_file` resolve against the same value,
/// which is what makes a command and a file call name the same file.
async fn execute(exec: &ExecCall, session: &Session) -> Response {
    let Some((program, args)) = exec.split() else {
        return Response::Error(refused(Error::INVALID_PARAMS, "an empty command"));
    };

    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(environment(session))
        // Piped and then read by `run_to_end`, which is what carries the output back.
        // Input is at EOF from the start, since an `exec` carries none.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Dropping `child` below kills and reaps the direct child, which is how a command
        // that timed out is cleaned up after the group kill has reached what it spawned —
        // and how one that ends this function early, by any other return, is not left
        // running.
        .kill_on_drop(true)
        // Its own process group, so a `killpg` here reaches everything the command started
        // — `sh -c 'a & b'` leaves two children of its own — and so that the group is this
        // command's alone: the `killpg` below never reaches this agent, which as the
        // guest's PID 1 is not a process a stray signal may end.
        .process_group(0);

    cmd.current_dir(session.cwd());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let code = match e.kind() {
                io::ErrorKind::NotFound => NOT_FOUND,
                _ => NOT_EXECUTABLE,
            };
            return Response::Error(refused(
                Error::NOT_EXECUTABLE,
                format!("{program}: {e} (a shell would report {code})"),
            ));
        }
    };
    // Taken now: a reaped child has no id, and the kill below wants the one it was born
    // with — which is also its group's, by `process_group(0)`.
    let pid = child.id();

    let waited = match exec.timeout_ms {
        None => run_to_end(&mut child).await,
        Some(ms) => {
            // `run_to_end` borrows the child rather than taking it, so expiry leaves
            // `child` — alive, unkilled and unreaped — in this frame for the kill below.
            match tokio::time::timeout(Duration::from_millis(ms), run_to_end(&mut child)).await {
                Ok(output) => output,
                Err(_elapsed) => {
                    // Everything the command started is in the group the spawn put it in,
                    // so one signal to the group ends all of it, the direct child included.
                    // `kill_on_drop` is not what does the killing here — dropping `child`
                    // on the way out is what reaps the leader once the signal has landed.
                    if let Some(pid) = pid {
                        // SAFETY: a plain libc call, and `pid` names this command's process
                        // group and no other. It is the group id this process created with
                        // `process_group(0)`, and it is still that group's: `timeout` only
                        // expires while `run_to_end` is pending, and while it is pending the
                        // direct child — the group's leader — has not been reaped (see
                        // `run_to_end`). An unreaped process keeps its pid, running or
                        // zombie, so the kernel cannot have handed this id to anything else.
                        unsafe {
                            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                        }
                    }
                    return Response::Error(refused(
                        Error::TIMED_OUT,
                        format!("killed after {ms}ms"),
                    ));
                }
            }
        }
    };

    finished(waited)
}

/// Drain both pipes to EOF, then reap the child — in that order, and the order is the point.
///
/// This is what `std`'s `wait_with_output` does and tokio's does not: tokio's waits and
/// reads concurrently, reaping the direct child the moment it exits while whatever it
/// spawned may still hold the pipes open. Reaping last keeps the direct child unreaped — a
/// zombie, once it has exited — for as long as anything is still writing, and an unreaped
/// process keeps its pid. Since that pid is also the id of the process group the command
/// was spawned into, the group id stays this command's until this future completes, which
/// is what lets [`execute`] aim a `killpg` at it after a timeout: the timeout can only fire
/// while this is pending, and while this is pending the leader is still there.
///
/// Both pipes are read at once because a command that fills a pipe nobody is reading stops
/// there, and read to EOF rather than to some size because what to keep of the result is
/// [`finished`]'s decision, not this function's.
async fn run_to_end(child: &mut Child) -> io::Result<Output> {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let (stdout, stderr) = tokio::try_join!(read_to_end(&mut stdout), read_to_end(&mut stderr))?;
    let status = child.wait().await?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Everything a pipe has to give, or nothing from a pipe that was never opened.
async fn read_to_end<R: AsyncRead + Unpin>(pipe: &mut Option<R>) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if let Some(pipe) = pipe {
        pipe.read_to_end(&mut buf).await?;
    }
    Ok(buf)
}

/// The environment variables an execution is given: whatever the image stated, plus the two
/// things this end has to answer for itself.
///
/// Nothing is inherited. libkrun hands the guest's first process only what the boot named, so
/// there is no ambient environment here to add to — which is why the image's own `ENV` has to
/// be replayed rather than assumed present. It is where a Debian-based Python image puts its
/// `PATH` and its `LANG`, and a command that runs without them runs somewhere the image was
/// never built to be.
///
/// The layering is image first, ours over the top. A `PATH` the image stated is a default and
/// stands; a guest whose image stated none gets [`GUEST_PATH`], because a guest where `sh`
/// cannot find `ls` is not a guest anything runs in. `PWD` is not a default at all — it has to
/// agree with where the command is actually standing.
///
/// Per command rather than per process, for the same reason a host-local console does it
/// that way: `std::env::set_var` is unsound with any other thread running, and this
/// program has one.
fn environment(session: &Session) -> Vec<(OsString, OsString)> {
    // Anything without a `=` is not a variable. The list comes out of an image's config, so
    // it is somebody else's data and the one malformed entry should not cost the rest.
    let mut env: Vec<(OsString, OsString)> = session
        .image
        .env
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect();

    if !env.iter().any(|(name, _)| name == "PATH") {
        env.push(("PATH".into(), GUEST_PATH.into()));
    }
    // Prepended, not appended: `/abin` is what cortex undertakes to provide, and an image
    // shadowing one of those names would make that undertaking untrue. Guarded on the
    // directory, because a session whose host had no executables to give it gets none.
    if std::path::Path::new(ABIN_PATH).is_dir()
        && let Some((_, path)) = env.iter_mut().find(|(name, _)| name == "PATH")
    {
        let mut with_abin = OsString::from(ABIN_PATH);
        with_abin.push(":");
        with_abin.push(&*path);
        *path = with_abin;
    }

    // `PWD` is what a shell reads to answer `pwd`, and a command spawned in the session's
    // directory would otherwise be told it was standing wherever this process is. Set to
    // match, which is what a shell does for itself when it moves.
    env.push(("PWD".into(), session.cwd().into()));
    env
}

/// A command that ended, as the answer to whatever asked for it.
fn finished(output: io::Result<Output>) -> Response {
    let out = match output {
        Ok(out) => out,
        Err(e) => {
            return Response::Error(refused(
                Error::INTERNAL_ERROR,
                format!("waiting for a command: {e}"),
            ));
        }
    };

    // A response travels in one frame under `MAX_PAYLOAD`, so a command that writes
    // without limit has to be cut off somewhere: an uncut one makes a frame this protocol
    // refuses to send, which is the whole session lost rather than one answer shortened.
    // Both streams, halved, because either of them alone can be the large one.
    let mut stdout = out.stdout;
    let mut stderr = out.stderr;
    let truncated = stdout.len() > MAX_STREAM || stderr.len() > MAX_STREAM;
    stdout.truncate(MAX_STREAM);
    stderr.truncate(MAX_STREAM);

    result(ExecResp {
        code: exit_code(&out.status),
        stdout,
        stderr,
        // Saying so is the point: an agent reading output it does not know is partial
        // draws a conclusion from it, and a wrong answer is worse than a short one.
        truncated,
    })
}

/// Hand back part of a file, as the answer to the `read` that asked for it.
///
/// The size is taken before the bytes are, so a file that grows between the two is
/// reported as the shorter one it was — and one that shrinks is answered with however
/// much was still there. Either way `data` is what was read and `size` is what the file
/// measured, which is the pair a requester needs to know whether to ask again.
async fn read_file(root: &Path, read: &ReadCall) -> Response {
    let path = at(root, &read.path);
    let path = path.as_path();

    let size = match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {
            return Response::Error(refused(
                Error::IS_A_DIRECTORY,
                format!("{}: is a directory", read.path),
            ));
        }
        Ok(meta) => meta.len(),
        Err(e) => return Response::Error(file_error(e, &read.path)),
    };

    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) => return Response::Error(file_error(e, &read.path)),
    };

    // Starting past the end asks for nothing, which is an empty answer rather than an
    // error: `size` is there to say what the offset was past.
    let offset = read.offset.unwrap_or(0);
    let left = size.saturating_sub(offset);
    let want = read.len.unwrap_or(left).min(left).min(MAX_DATA) as usize;

    // The handle is this call's alone, so a cursor is as good as a position on every
    // read: nothing else can move it.
    if let Err(e) = file.seek(SeekFrom::Start(offset)).await {
        return Response::Error(file_error(e, &read.path));
    }

    let mut data = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        match file.read(&mut data[filled..]).await {
            // The file lost the bytes its size promised, so what is here is all there is.
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Response::Error(file_error(e, &read.path)),
        }
    }
    data.truncate(filled);

    Response::Read(ReadResp { data, size })
}

/// Put bytes in a file, as the answer to the `write` that sent them.
///
/// No offset means the file is to *be* `data`, which is what `create` does: made if it
/// was not there, cut to nothing if it was. An offset means only those bytes are being
/// spoken for, so the file is opened without truncating and whatever lies past them
/// stays.
async fn write_file(root: &Path, write: &WriteCall) -> Response {
    let path = at(root, &write.path);
    let path = path.as_path();

    let file = match write.offset {
        None => tokio::fs::File::create(path).await,
        Some(_) => {
            tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .await
        }
    };

    let mut file = match file {
        Ok(file) => file,
        Err(e) => return Response::Error(file_error(e, &write.path)),
    };

    // Seeking past the end and writing there is what leaves zeroes in the gap, the same
    // as a positioned write would.
    if let Err(e) = file.seek(SeekFrom::Start(write.offset.unwrap_or(0))).await {
        return Response::Error(file_error(e, &write.path));
    }

    if let Err(e) = file.write_all(&write.data).await {
        return Response::Error(file_error(e, &write.path));
    }

    // Before the size is asked for, because these writes are buffered and a size taken
    // over a buffer that has not gone out is the size the file used to be.
    if let Err(e) = file.flush().await {
        return Response::Error(file_error(e, &write.path));
    }

    match file.metadata().await {
        Ok(meta) => Response::Write(WriteResp { size: meta.len() }),
        Err(e) => Response::Error(file_error(e, &write.path)),
    }
}

/// What went wrong with a file, as one of the codes a requester branches on.
///
/// Everything that is not the path being absent or being a directory is
/// [`IO_FAILED`](Error::IO_FAILED): permissions, a full overlay, a name too long. The
/// message carries what the guest kernel said, since that is the part worth reading.
fn file_error(e: io::Error, path: &str) -> Error {
    let code = match e.kind() {
        io::ErrorKind::NotFound => Error::NOT_FOUND,
        io::ErrorKind::IsADirectory => Error::IS_A_DIRECTORY,
        _ => Error::IO_FAILED,
    };
    refused(code, format!("{path}: {e}"))
}

/// Everything flushing this session's writes needs to know, worked out while the session is
/// still in hand.
///
/// Separated from the doing so that the doing can be handed to a thread: what it takes is a
/// few paths, where a [`Session`] is neither `Send` nor something to hold across one.
enum SnapshotPlan {
    /// No layer was arranged for at boot, so the disk is the whole of what there is to flush.
    DiskOnly,
    Write {
        into: PathBuf,
        excluded: Vec<PathBuf>,
        mount_points: Vec<PathBuf>,
    },
}

impl SnapshotPlan {
    /// Put this session's writes where the host can read them.
    ///
    /// The answer carries no blob, because what the host reads is the disk behind
    /// `/dev/vda` and this end holds no file for it. What it can do is make sure every
    /// write has reached that device, which is what a `sync` is for here.
    fn run(self) -> Response {
        if let SnapshotPlan::Write {
            into,
            excluded,
            mount_points,
        } = self
            && let Err(e) =
                crate::layer::write(Path::new(UPPER_DIR), &into, &excluded, &mount_points)
        {
            return Response::Error(refused(
                Error::IO_FAILED,
                format!("writing this session's layer: {e}"),
            ));
        }

        // Every dirty page down to the block device before anything on the other side of it
        // reads the file it is backed by. Without this a snapshot is whatever of the session
        // happened to have been written back.
        unsafe { libc::sync() };
        Response::Snapshot(SnapshotResp { blob: Vec::new() })
    }
}

/// What this session would leave behind, and where.
///
/// A layer needs two things arranged while booting — the root holding this overlay's upper,
/// and somewhere to write the result — and a session that did not say it might be built into
/// an image has neither. The disk is flushed either way, which is what a snapshot is.
fn snapshot_plan(session: &Session) -> SnapshotPlan {
    let Ok(commit) = std::env::var(COMMIT_ENV) else {
        return SnapshotPlan::DiskOnly;
    };

    // Everything the console server and this agent put in the session's own filesystem. None
    // of it is the session's work, and one of them is this binary.
    let mut excluded: Vec<PathBuf> = [GUEST_BIN_PATH, "/oldroot", ABIN_PATH, COMMIT_PATH]
        .iter()
        .map(|path| PathBuf::from(path.trim_start_matches('/')))
        .collect();
    // Written into the session while it was up, by the network setup rather than by anything
    // the session did. Docker leaves this file out of a commit too, and for the same reason.
    //
    // Named by the constant the writer uses rather than spelled again: two spellings of one
    // path are two that drift, and the way this one would drift is silently — a committed
    // image carrying a nameserver that belonged to a VM which no longer exists.
    excluded.push(inside_upper(Path::new(crate::contract::RESOLV_CONF)));

    // A mount point rather than a plain exclusion: the directories on the way to one exist
    // only to reach it, and go with it.
    let mount_points: Vec<PathBuf> = session
        .context
        .as_deref()
        .map(|path| vec![inside_upper(path)])
        .unwrap_or_default();

    SnapshotPlan::Write {
        into: Path::new(&commit).join(LAYER_TAR),
        excluded,
        mount_points,
    }
}

/// An absolute path in the guest, as the path it has inside the upperdir.
fn inside_upper(path: &Path) -> PathBuf {
    path.strip_prefix("/").unwrap_or(path).to_path_buf()
}

/// An execution that ended, as the answer to the `exec` that asked for it.
fn result(ended: ExecResp) -> Response {
    Response::Exec(ended)
}

/// The code to report for a command that ran, the way a shell would.
///
/// The one part of an [`ExitStatus`] that is not just `status.code()`: a command killed
/// by a signal has no code of its own, and the convention is `128 + signal`.
fn exit_code(status: &ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(NOT_EXECUTABLE)
}

/// A refusal, as the `error` a response carries instead of a result.
fn refused(code: i64, message: impl Into<String>) -> Error {
    Error::new(code, message)
}
