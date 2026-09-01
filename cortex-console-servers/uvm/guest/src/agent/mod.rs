//! The agent role: answer a console session by running commands in this guest.
//!
//! What arrives here has come a long way — a client's `exec`, framed on the host, handed
//! to a virtio-console port, read out of a character device in here — and none of that is
//! visible below. A [`Server`] over the port brings requests in and puts responses out,
//! and what is left is what is actually this end's: making a delegated name runnable,
//! running a command, letting a delegated call back out while that command waits for it,
//! and reading and writing the files a command works with.
//!
//! Which is the same list [`cortex-local-console`]'s server role has, answered the same
//! way. That is worth stating rather than hiding: the two backends differ in *where* a
//! command runs and in nothing else, and the code below being recognisable is the shape
//! of that fact rather than an accident. What should eventually be shared is shared —
//! see [What is duplicated](#what-is-duplicated).
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
//! # Running the command, and pausing it
//!
//! [`Command`], captured — but spawned rather than run to completion, because a delegated
//! call arrives *while* it runs and has to be answered before it can finish. So an
//! execution is a [`select!`](tokio::select) over the two things that can happen next: a
//! shim connects, or the command ends.
//!
//! The command is waited for in a task of its own rather than in a branch of that
//! `select!`. Waiting for it means draining its stdout and stderr as they fill, and a
//! command that fills a pipe while nobody is reading it stops there — which would happen
//! every time an execution paused on a delegated call, since answering one is round trips
//! on the console channel and not polling of anything else. A task keeps the pipes moving
//! throughout, and what the `select!` waits on is the task ending.
//!
//! Shims first, when both are ready: a connection that arrived is a delegated call that
//! has not been answered, and the process behind it is still waiting to hear. Reporting
//! the command's ending while one sat unserved would strand it.
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
//! The interleaving loop in [`execute`], the shim socket, the bin directory and the two
//! file calls are what a host-local console does, in a guest. They are written out again
//! here rather than shared, and that is a debt with a known payment: the pieces belong in
//! cortex's console module, as helpers a backend calls, so that a backend implements only
//! what actually differs — where a command runs, what booting means, what has to be
//! released. Until then, a fix to one of them is a fix owed to the other.
//!
//! # What this does not do
//!
//! A delegated name is linked **without being checked as a plain path component**, the
//! same gap [`cortex-local-console`] has and for the same reason: every name that gets
//! here came from the client, which is in-process with whoever chose them.
//!
//! A `read` or a `write` is not confined to anywhere. The path is used as it arrives, so
//! a client can name any file in the guest — which is a smaller claim than it sounds,
//! since the guest is one session's overlay and the client is the one who asked for it.
//!
//! Neither timeout is enforced. An `exec` carries a `timeout_ms` and an `init` carries
//! the fallback, and this agent reads both and applies neither — so a command that never
//! ends is one this agent waits on forever, and the client waits with it. What it takes
//! is a bound around the `select!` in [`execute`] and an answer for what a delegated call
//! already in flight becomes when it expires, which is the part that is a decision and
//! not a line of code.
//!
//! [`cortex-local-console`]: https://docs.rs/cortex-local-console

mod bin_dir;
mod commit;

use std::ffi::OsString;
use std::io::{self, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};

use bson::Bson;
use cortex::console::stdio::StdioServer;
use cortex::console::{
    Call, Error, Exec, ExecCmd, ExecResult, Init, InitResult, MAX_PAYLOAD, Message, Notification,
    Outcome, Progress, Read, ReadResult, RequestId, Server, WorkFsMount, WorkFsSource, Write,
    WriteResult,
};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;

use crate::contract::{
    ABIN_PATH, COMMIT_ENV, COMMIT_PATH, GUEST_BIN_PATH, GUEST_PATH, GuestCommit, HANDSHAKE,
    ImageSpec, LAYER_TAR, UPPER_DIR,
};
use crate::ipc::SOCK_ENV;
use bin_dir::BinDir;

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

/// Answer requests on `port` until the client says `quit` or the channel closes.
///
/// `port` is the virtio-console port, open read-write. It becomes two halves here, from
/// one `dup`: the driver refuses a second opener, and the framing wants to read and write
/// through borrows that do not have to take turns.
///
/// `image` is what the base image stated, read by [`init`](crate::init) while its file was
/// still reachable. It is the session's for the whole process because it describes the root
/// every session in here runs on, which no `init` can change.
pub async fn run(port: std::fs::File, image: ImageSpec) -> anyhow::Result<()> {
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

    // Bound once, before anything can be running, and kept for the process. See
    // [`Shims::bind`] for why its lifetime is the process's and not a session's.
    let shims = Shims::bind()?;

    // Whatever it holds is dropped on `stop` and on the way out of this function
    // whichever way it leaves, which takes the symlinks with it every time.
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
                if let Err(Outcome::Error(e)) = session.boot() {
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
                    let outcome = match session.configure(init) {
                        Ok(answer) => encoded(bson::serialize_to_bson(&answer)),
                        Err(outcome) => outcome,
                    };
                    server.respond(id, outcome).await?;
                }

                // An `exec` whose `cmd` is an answer rather than a command is read
                // inside `execute`, by the execution that is waiting for it. Reaching the
                // main loop means nothing is: the client is carrying on from an execution
                // this end is not holding.
                Call::Exec(exec) if matches!(exec.cmd, ExecCmd::Resume { .. }) => {
                    server
                        .respond(
                            id,
                            refused(
                                Error::INVALID_REQUEST,
                                "nothing is waiting on a delegated call, so there is nothing \
                                 to carry on from",
                            ),
                        )
                        .await?
                }

                Call::Exec(exec) => match session.boot() {
                    Err(outcome) => server.respond(id, outcome).await?,
                    // `cd` is the session's own and not a program: see
                    // [`Session::change_dir`]. It is answered here rather than spawned,
                    // and it is the one thing that moves where the session stands.
                    Ok(()) => match cd_target(&exec) {
                        Some(argv) => {
                            let outcome = session.change_dir(argv);
                            server.respond(id, outcome).await?;
                        }
                        None => execute(&mut server, id, &exec, &session, &shims).await?,
                    },
                },

                Call::Read(read) => {
                    let outcome = match session.boot() {
                        Ok(()) => read_file(session.cwd(), &read).await,
                        Err(outcome) => outcome,
                    };
                    server.respond(id, outcome).await?;
                }

                Call::Write(write) => {
                    let outcome = match session.boot() {
                        Ok(()) => write_file(session.cwd(), &write).await,
                        Err(outcome) => outcome,
                    };
                    server.respond(id, outcome).await?;
                }

                Call::Commit(_) => {
                    let outcome = match session.boot() {
                        Ok(()) => write_layer(&session),
                        Err(outcome) => outcome,
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

/// What a session is in here: what the client announced, where it stands, and whatever
/// booting it took.
///
/// The three are apart because they are wanted at different moments. What [`Init`] carries
/// is the session's shape and costs nothing to hold; where it stands is a path and costs no
/// more; the directory of symlinks is a real thing on the overlay, and the point of `start`
/// and `stop` being optional is that it may come and go underneath a session that does not
/// change.
///
/// **The same shape the host-local backend has**, deliberately: a client cannot tell which
/// one answered it, so what a session *is* must not depend on which one did. What differs
/// between them is only where a command runs, which is the whole of what a backend is for.
#[derive(Default)]
struct Session {
    /// What `init` said, or the defaults for a client that never sent one — which is a
    /// session with nothing delegated, and that is a session.
    config: Init,

    /// The tree the session works in, as `init` named it — and as it is mounted in here.
    ///
    /// The same path on both sides of the hypervisor: the boot shares the host's directory
    /// at its own path, so what the client was told and what this end opens are one string.
    /// See `crate::init::share`.
    workfs: Option<PathBuf>,

    /// Where the session stands, which is what an execution runs in and what a relative
    /// path in a file call resolves against.
    cwd: Option<PathBuf>,

    /// What the base image stated. A property of the root rather than of the session, so an
    /// `init` neither sets it nor clears it.
    image: ImageSpec,

    /// `None` until something boots it: a `start`, or the first call that needs one.
    linked: Option<BinDir>,
}

impl Session {
    /// A session on the root the image describes, with nothing configured yet.
    fn new(image: ImageSpec) -> Session {
        Session {
            image,
            ..Session::default()
        }
    }

    /// Take a new shape, answering what the client has to know about it.
    ///
    /// The URL is read before anything is let go of, so a session this agent cannot take is
    /// one it has not taken. Otherwise the old boot goes: the delegated names are built into
    /// the directory, and a boot from before this is a boot that no longer matches.
    fn configure(&mut self, config: Init) -> Result<InitResult, Outcome> {
        let workfs = config.workfs.as_ref().map(directory_url).transpose()?;

        self.release();
        // A session with no tree stands where this process was put, which `init::prepare`
        // set to `/`. Saying so beats leaving the client to guess what a relative path means.
        self.cwd = workfs.clone().or_else(|| std::env::current_dir().ok());
        self.workfs = workfs;
        self.config = config;

        Ok(InitResult {
            workfs: self.workfs.as_deref().map(|path| WorkFsMount {
                path: path.to_string_lossy().into_owned(),
            }),
            cwd: self.named_cwd(),
            // Nothing to say: this agent is running *inside* the base and never heard which
            // one it is. The answer the client sees is the console server's, one boot up.
            image: None,
            // Not this end's to say. What it has is an interface or no interface; which reach
            // that interface is behind is a policy on the far side of the device, and the host
            // is what answers the client about it.
            network: None,
        })
    }

    /// Where the session stands.
    fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// The same, as the protocol can carry it.
    fn named_cwd(&self) -> Option<String> {
        self.cwd().and_then(|cwd| cwd.to_str().map(str::to_owned))
    }

    /// What booting produced, for the one thing outside this type that needs it.
    fn linked(&self) -> Option<&BinDir> {
        self.linked.as_ref()
    }

    fn release(&mut self) {
        self.linked = None;
    }

    /// Bring the session up if nothing has: the tree where `init` said it would be, and the
    /// delegated names somewhere `execvp` will find them.
    ///
    /// The tree was mounted before this process started — `init::prepare` did it, from what
    /// the boot named — so realizing it here is checking that it is there, which is what
    /// [`MOUNT_FAILED`](Error::MOUNT_FAILED) reports when it is not. A guest whose share
    /// failed to mount is a session described correctly in an environment that is wrong for
    /// it, which is exactly that code's case.
    ///
    /// The directory of symlinks is not put on `PATH`: every execution is given its own
    /// environment (see [`environment`]), which is what an inherited `PATH` and a `set_var`
    /// would otherwise be for — and doing it per command rather than per process is what
    /// lets this program have more than one thread.
    fn boot(&mut self) -> Result<(), Outcome> {
        if self.linked.is_some() {
            return Ok(());
        }

        if let Some(workfs) = &self.workfs {
            match std::fs::metadata(workfs) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    return Err(refused(
                        Error::MOUNT_FAILED,
                        format!("{}: not a directory in the guest", workfs.display()),
                    ));
                }
                Err(e) => {
                    return Err(refused(
                        Error::MOUNT_FAILED,
                        format!("{}: not mounted in the guest: {e}", workfs.display()),
                    ));
                }
            }
        }

        let linked = BinDir::create(self.config.delegated.iter().map(String::as_str))
            .map_err(boot_failed)?;
        self.linked = Some(linked);
        Ok(())
    }

    /// Move where the session stands, and say where to.
    ///
    /// **`cd` is the session's own because it is nobody else's.** It is a shell builtin and
    /// not a program, so an `exec` carrying an argv has no other way to offer it — and the
    /// shell a person would be talking to is, here, the session itself. A `cd` inside a
    /// command (`sh -c 'cd x'`) moves that command's own process and nothing else, which is
    /// what a subshell does in a terminal too.
    ///
    /// The answer is a `done` and not an error, including when it fails: `cd` behaves like
    /// the builtin it stands in for.
    ///
    /// **Logical, like a shell's own.** The path is normalized on paper and symlinks are
    /// left alone, so what goes back is spelled the way `init` answered — which here is also
    /// the way the *host* spells it, since the tree is mounted at the host's own path.
    fn change_dir(&mut self, argv: &[String]) -> Outcome {
        let target = match argv {
            [] => match self.workfs.clone().or_else(|| self.cwd.clone()) {
                Some(home) => home,
                None => return builtin_failed("cd: this session stands nowhere to return to"),
            },
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

        self.cwd = Some(moved);
        result(Progress::Done(ExecResult {
            code: 0,
            cwd: self.named_cwd(),
            ..ExecResult::default()
        }))
    }
}

/// The argv of an `exec` that is a `cd`, without the name — or `None` for anything else.
fn cd_target(exec: &Exec) -> Option<&[String]> {
    match exec.cmd.split() {
        Some((program, args)) if program == "cd" => Some(args),
        _ => None,
    }
}

/// The directory a workfs URL names, or why it names none this agent can use.
///
/// `file://` and nothing else. Inside a guest that is not a limitation the way it is on the
/// host: whatever the tree is made of was realized before the VM started, and what reaches
/// here is the directory it was mounted at.
fn directory_url(workfs: &WorkFsSource) -> Result<PathBuf, Outcome> {
    let Some(path) = workfs.file_path() else {
        return Err(refused(
            Error::UNSUPPORTED_WORKFS,
            format!(
                "{}: a guest is handed a mounted directory, so file:// is the only kind it \
                 can be told about",
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
fn at(root: Option<&Path>, path: impl AsRef<Path>) -> PathBuf {
    match root {
        Some(root) => root.join(path),
        None => path.as_ref().to_path_buf(),
    }
}

/// A builtin that ran and failed, the way the shell builtin it stands in for would: a code
/// and a line, and not a refusal of the call.
fn builtin_failed(message: impl Into<String>) -> Outcome {
    let mut stderr: Vec<u8> = message.into().into_bytes();
    stderr.push(b'\n');
    result(Progress::Done(ExecResult {
        code: 1,
        stderr,
        ..ExecResult::default()
    }))
}

/// A boot that did not happen, as the answer to whatever needed one.
fn boot_failed(e: io::Error) -> Outcome {
    refused(Error::BOOT_FAILED, format!("linking delegated names: {e}"))
}

/// Run one command, answering the console channel as many times as it takes.
///
/// Exactly one of those answers is the execution's own — a [`Done`](Progress::Done) or an
/// error — and it goes to whichever request is owed one by then: the `exec` a caller
/// asked for if nothing was delegated, or the last `exec` that carried an answer back if
/// something was.
///
/// # Where it runs
///
/// Where the session stands, set per command rather than inherited. A session moves — `cd`
/// is answered by [`Session::change_dir`] and the next command runs where it left off — so
/// this process's own working directory would be a second answer to that question, right
/// only until the first `cd`. `read_file` and `write_file` resolve against the same value,
/// which is what makes a command and a file call name the same file.
async fn execute(
    server: &mut StdioServer,
    id: RequestId,
    exec: &Exec,
    session: &Session,
    shims: &Shims,
) -> io::Result<()> {
    let Some((program, args)) = exec.cmd.split() else {
        return server
            .respond(id, refused(Error::INVALID_PARAMS, "an empty command"))
            .await;
    };

    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(environment(session, shims))
        // Piped and then read by `wait_with_output`, which is what carries the output
        // back. Input is at EOF from the start, since an `exec` carries none.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(dir) = session.cwd() {
        cmd.current_dir(dir);
    }

    let child = cmd.spawn();

    let child = match child {
        Ok(child) => child,
        Err(e) => {
            let code = match e.kind() {
                io::ErrorKind::NotFound => NOT_FOUND,
                _ => NOT_EXECUTABLE,
            };
            return server
                .respond(
                    id,
                    refused(
                        Error::NOT_EXECUTABLE,
                        format!("{program}: {e} (a shell would report {code})"),
                    ),
                )
                .await;
        }
    };

    // In a task of its own, so both pipes keep draining while this function is busy
    // answering a delegated call. See the module docs.
    let mut running = tokio::spawn(child.wait_with_output());

    // Which request this execution owes its answer to. The `exec` a caller asked for to
    // begin with, and each `exec` carrying an answer back after that — a `Delegated`
    // spends the one it is sent on.
    let mut owed = id;

    loop {
        tokio::select! {
            // A shim that has already connected is a delegated call already waiting, so
            // it is served before an ending is reported.
            biased;

            accepted = shims.accept() => match accepted {
                Some(stream) => match delegate(server, owed, stream).await? {
                    Some(next) => owed = next,
                    // The client stopped saying anything that could carry the execution
                    // on, so there is nobody left to answer. The command is left to the
                    // process's ending, which is moments away: the main loop reads the
                    // same channel.
                    None => return Ok(()),
                },
                // The socket is gone, so no delegated call will ever arrive again. The
                // command still can end, which is the only thing left to wait for.
                None => return server.respond(owed, finished(join(&mut running).await)).await,
            },

            output = &mut running => {
                return server.respond(owed, finished(joined(output))).await;
            }
        }
    }
}

/// Hand one delegated call to the client, and give the shim what comes back.
///
/// Two messages on the console channel: the [`Delegated`](Progress::Delegated) that
/// answers what this execution currently owes, and the `exec` whose `cmd` carries the
/// answer back. What is returned is the id of the request now owed the execution's own
/// answer, or `None` when the client said nothing that could be one.
async fn delegate(
    server: &mut StdioServer,
    owed: RequestId,
    stream: UnixStream,
) -> io::Result<Option<RequestId>> {
    // Owned halves, because the two directions are separate fields of a `StdioServer` and
    // have to outlive the borrow the stream came in on.
    let (incoming, outgoing) = stream.into_split();
    let mut shim = StdioServer::new(incoming, outgoing);

    // A connection carrying anything but a shim's one `exec` is not something to forward,
    // and the execution still owes what it owed. Dropping the connection is all there is
    // to say to it.
    let (shim_id, exec) = match shim.recv().await? {
        Some(Message::Request {
            id,
            call: Call::Exec(exec),
        }) => (id, exec),
        other => {
            eprintln!(
                "{}: a shim sent {other:?} instead of an exec",
                env!("CARGO_BIN_NAME")
            );
            return Ok(Some(owed));
        }
    };

    // Verbatim, `cwd` and `env` and all. The tree is mounted here at the path the host
    // spells it with, so a directory inside it is already a name the client can use and
    // there is nothing to translate — which is the whole reason the share lands where it
    // does rather than at a constant of this crate's choosing.
    server
        .respond(owed, result(Progress::Delegated(exec)))
        .await?;

    // Only the answer to what was just asked for can arrive now: an `exec` whose `cmd`
    // names the request the `Delegated` above went out on. This end owes an answer it has
    // not sent, so there is nothing else the client could be asking about — and a `cmd`
    // naming anything else is a client that has lost its place, which is the whole of what
    // that id is for.
    let carried = match server.recv().await? {
        Some(Message::Request {
            id,
            call: Call::Exec(exec),
        }) => match exec.cmd {
            ExecCmd::Resume { id: from, outcome } if from == owed => Some((id, outcome)),
            _ => None,
        },
        _ => None,
    };

    let Some((id, outcome)) = carried else {
        // The shim goes unanswered on purpose. There is nothing to tell it, and a dropped
        // connection is what says so — the shim reports that on its own stderr.
        eprintln!(
            "{}: the client sent nothing that carries on from request {owed}",
            env!("CARGO_BIN_NAME")
        );
        return Ok(None);
    };

    // Whatever the client said, verbatim: a refusal is as much an answer as a result, and
    // the shim is what turns either into an exit code.
    shim.respond(shim_id, outcome).await?;
    Ok(Some(id))
}

/// The environment variables an execution is given: whatever the image stated, the way home
/// for a shim, and a `PATH` with the delegated names on it.
///
/// Nothing is inherited. libkrun hands the guest's first process only what the boot named, so
/// there is no ambient environment here to add to — which is why the image's own `ENV` has to
/// be replayed rather than assumed present. It is where a Debian-based Python image puts its
/// `PATH` and its `LANG`, and a command that runs without them runs somewhere the image was
/// never built to be.
///
/// The layering is image first, ours over the top. A variable the image set is a default; the
/// two we set are not — [`SOCK_ENV`] is how a shim finds its way back to this process, and
/// `PWD` has to agree with where the command is actually standing.
///
/// Per command rather than per process, for the same reason a host-local console does it
/// that way: `std::env::set_var` is unsound with any other thread running, and this
/// program has one.
fn environment(session: &Session, shims: &Shims) -> Vec<(OsString, OsString)> {
    // Anything without a `=` is not a variable. The list comes out of an image's config, so
    // it is somebody else's data and the one malformed entry should not cost the rest.
    let mut env: Vec<(OsString, OsString)> = session
        .image
        .env
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect();

    // The image's `PATH` when it gave one, ours when it did not — see [`GUEST_PATH`]. Either
    // way the delegated names are appended and not prepended: they are meant to add commands,
    // not to quietly shadow a real `git` or `python` the image ships.
    let mut path = match env.iter().position(|(name, _)| name == "PATH") {
        Some(stated) => env.remove(stated).1,
        None => OsString::from(GUEST_PATH),
    };
    if let Some(linked) = session.linked() {
        path.push(":");
        path.push(linked.bin());
    }
    // Prepended, unlike the delegated names above. A delegated name is one the client chose
    // and the image should win a clash; `/abin` is what cortex undertakes to provide, and an
    // image shadowing it would make that undertaking untrue.
    if std::path::Path::new(ABIN_PATH).is_dir() {
        let mut with_abin = OsString::from(ABIN_PATH);
        with_abin.push(":");
        with_abin.push(&path);
        path = with_abin;
    }

    env.push((SOCK_ENV.into(), shims.sock.clone().into_os_string()));
    env.push(("PATH".into(), path));

    // `PWD` is what a shell reads to answer `pwd`, and a command spawned in the session's
    // directory would otherwise be told it was standing wherever this process is. Set to
    // match, which is what a shell does for itself when it moves.
    if let Some(cwd) = session.cwd() {
        env.push(("PWD".into(), cwd.into()));
    }
    env
}

/// The socket every shim dials.
///
/// # Why the process binds it and not a session
///
/// The path is in the environment of everything an execution spawns, and a socket bound
/// per `start` would be a path that changes under anything long-lived a previous session
/// left behind. One socket for the process is one answer to "where do I dial", for as
/// long as there is a process to dial into.
///
/// Nothing accepts on it except an execution, and only while one is running. A shim that
/// connects at any other moment — between a `stop` and the next `start`, or between two
/// commands — waits in the socket's backlog until something accepts it or this process
/// goes away. Which is what a shim does anyway: it is a program waiting for its own
/// output, and it has nothing else to do.
struct Shims {
    /// Handed to every execution's environment, which is how a shim finds it.
    sock: PathBuf,

    listener: UnixListener,
}

impl Shims {
    fn bind() -> anyhow::Result<Shims> {
        let sock = std::env::temp_dir().join(format!("cortex-console-{}.sock", std::process::id()));
        // A live pid cannot have left one behind, so anything here is from an earlier
        // boot of this session: the overlay persists across the VMs, the pids do not.
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)?;

        Ok(Shims { sock, listener })
    }

    /// The next shim to connect, or `None` if none ever will again.
    ///
    /// Cancel-safe, which is what lets this sit in a `select!` that an ending command may
    /// win: a connection is either accepted whole or not accepted at all, and one left in
    /// the backlog is still there for the next execution.
    ///
    /// A failed accept is not the end of anything by itself — a peer that hung up between
    /// connecting and being accepted is one — so it is reported and waited past. What ends
    /// this is the listener itself failing, which no later accept would survive either.
    async fn accept(&self) -> Option<UnixStream> {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => return Some(stream),
                Err(e) if transient(&e) => {
                    eprintln!("{}: a shim went away: {e}", env!("CARGO_BIN_NAME"))
                }
                Err(e) => {
                    eprintln!("{}: no more shims: {e}", env!("CARGO_BIN_NAME"));
                    return None;
                }
            }
        }
    }
}

impl Drop for Shims {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock);
    }
}

/// Whether an `accept` failure was about the one connection rather than the listener.
fn transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionAborted | io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
}

/// Wait for the task that is collecting a command, once there is nothing else to wait
/// for.
async fn join(running: &mut tokio::task::JoinHandle<io::Result<Output>>) -> io::Result<Output> {
    joined(running.await)
}

/// What the collecting task came back with.
///
/// Two layers: whether the task itself got to finish, and whether waiting for the command
/// worked. A task that did not finish is this process being torn down or a panic in the
/// waiting itself; either way there is no output to report and the execution has to say
/// so.
fn joined(joined: Result<io::Result<Output>, tokio::task::JoinError>) -> io::Result<Output> {
    joined.unwrap_or_else(|e| Err(io::Error::other(format!("collecting a command: {e}"))))
}

/// A command that ended, as the answer to whatever asked for it.
fn finished(output: io::Result<Output>) -> Outcome {
    let out = match output {
        Ok(out) => out,
        Err(e) => return refused(Error::INTERNAL_ERROR, format!("waiting for a command: {e}")),
    };

    result(Progress::Done(ExecResult {
        code: exit_code(&out.status),
        stdout: out.stdout,
        stderr: out.stderr,
        truncated: false,
        // A spawned command cannot have moved the session: `cd` in a child dies with the
        // child, and the one `cd` that moves anything here is the builtin, which never
        // reaches this function. See [`Session::change_dir`].
        cwd: None,
    }))
}

/// Hand back part of a file, as the answer to the `read` that asked for it.
///
/// The size is taken before the bytes are, so a file that grows between the two is
/// reported as the shorter one it was — and one that shrinks is answered with however
/// much was still there. Either way `data` is what was read and `size` is what the file
/// measured, which is the pair a requester needs to know whether to ask again.
async fn read_file(root: Option<&Path>, read: &Read) -> Outcome {
    let path = at(root, &read.path);
    let path = path.as_path();

    let size = match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {
            return refused(
                Error::IS_A_DIRECTORY,
                format!("{}: is a directory", read.path),
            );
        }
        Ok(meta) => meta.len(),
        Err(e) => return file_error(e, &read.path),
    };

    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) => return file_error(e, &read.path),
    };

    // Starting past the end asks for nothing, which is an empty answer rather than an
    // error: `size` is there to say what the offset was past.
    let offset = read.offset.unwrap_or(0);
    let left = size.saturating_sub(offset);
    let want = read.len.unwrap_or(left).min(left).min(MAX_DATA) as usize;

    // The handle is this call's alone, so a cursor is as good as a position on every
    // read: nothing else can move it.
    if let Err(e) = file.seek(SeekFrom::Start(offset)).await {
        return file_error(e, &read.path);
    }

    let mut data = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        match file.read(&mut data[filled..]).await {
            // The file lost the bytes its size promised, so what is here is all there is.
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return file_error(e, &read.path),
        }
    }
    data.truncate(filled);

    encoded(bson::serialize_to_bson(&ReadResult { data, size }))
}

/// Put bytes in a file, as the answer to the `write` that sent them.
///
/// No offset means the file is to *be* `data`, which is what `create` does: made if it
/// was not there, cut to nothing if it was. An offset means only those bytes are being
/// spoken for, so the file is opened without truncating and whatever lies past them
/// stays.
async fn write_file(root: Option<&Path>, write: &Write) -> Outcome {
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
        Err(e) => return file_error(e, &write.path),
    };

    // Seeking past the end and writing there is what leaves zeroes in the gap, the same
    // as a positioned write would.
    if let Err(e) = file.seek(SeekFrom::Start(write.offset.unwrap_or(0))).await {
        return file_error(e, &write.path);
    }

    if let Err(e) = file.write_all(&write.data).await {
        return file_error(e, &write.path);
    }

    // Before the size is asked for, because these writes are buffered and a size taken
    // over a buffer that has not gone out is the size the file used to be.
    if let Err(e) = file.flush().await {
        return file_error(e, &write.path);
    }

    match file.metadata().await {
        Ok(meta) => encoded(bson::serialize_to_bson(&WriteResult { size: meta.len() })),
        Err(e) => file_error(e, &write.path),
    }
}

/// What went wrong with a file, as one of the codes a requester branches on.
///
/// Everything that is not the path being absent or being a directory is
/// [`IO_FAILED`](Error::IO_FAILED): permissions, a full overlay, a name too long. The
/// message carries what the guest kernel said, since that is the part worth reading.
fn file_error(e: io::Error, path: &str) -> Outcome {
    let code = match e.kind() {
        io::ErrorKind::NotFound => Error::NOT_FOUND,
        io::ErrorKind::IsADirectory => Error::IS_A_DIRECTORY,
        _ => Error::IO_FAILED,
    };
    refused(code, format!("{path}: {e}"))
}

/// Write this session's layer where the host can read it.
///
/// The answer carries no image, because this end has no idea what one is: it has written a tar
/// and that is the whole of what it knows. The console server turns it into one.
///
/// A commit needs two things arranged while booting — the root holding this overlay's upper,
/// and somewhere to write the result — and a session that did not say it might commit has
/// neither. It cannot be given them now, so it is told rather than half-answered.
fn write_layer(session: &Session) -> Outcome {
    let Ok(scratch) = std::env::var(COMMIT_ENV) else {
        return refused(
            Error::INVALID_REQUEST,
            "this session was not booted able to commit",
        );
    };

    // Everything the console server and this agent put in the session's own filesystem. None
    // of it is the session's work, and one of them is this binary.
    let mut excluded: Vec<PathBuf> = [GUEST_BIN_PATH, "/oldroot", ABIN_PATH, COMMIT_PATH]
        .iter()
        .map(|path| PathBuf::from(path.trim_start_matches('/')))
        .collect();
    // Written into the session while it was up, by the network setup rather than by anything
    // the session did. Docker leaves this file out of a commit too, and for the same reason.
    excluded.push(PathBuf::from("etc/resolv.conf"));
    if let Some(linked) = session.linked() {
        excluded.push(inside_upper(linked.root()));
    }

    // A mount point rather than a plain exclusion: the directories on the way to one exist
    // only to reach it, and go with it.
    let mount_points: Vec<PathBuf> = session
        .workfs
        .as_deref()
        .map(|path| vec![inside_upper(path)])
        .unwrap_or_default();

    let into = Path::new(&scratch).join(LAYER_TAR);
    match commit::write_layer(Path::new(UPPER_DIR), &into, &excluded, &mount_points) {
        Ok(size) => encoded(bson::serialize_to_bson(&GuestCommit { size })),
        Err(e) => refused(
            Error::IO_FAILED,
            format!("writing this session's layer: {e}"),
        ),
    }
}

/// An absolute path in the guest, as the path it has inside the upperdir.
fn inside_upper(path: &Path) -> PathBuf {
    path.strip_prefix("/").unwrap_or(path).to_path_buf()
}

fn result(progress: Progress) -> Outcome {
    encoded(bson::serialize_to_bson(&progress))
}

/// A `result` from something that had to be encoded to become one.
fn encoded(value: Result<Bson, bson::error::Error>) -> Outcome {
    match value {
        Ok(value) => Outcome::Result(value),
        Err(e) => refused(Error::INTERNAL_ERROR, format!("encoding a result: {e}")),
    }
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

fn refused(code: i64, message: impl Into<String>) -> Outcome {
    Outcome::Error(Error::new(code, message))
}
