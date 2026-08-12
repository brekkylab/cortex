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

use std::ffi::OsString;
use std::io::{self, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};

use bson::Bson;
use cortex::console::stdio::StdioServer;
use cortex::console::{
    Call, Error, Exec, ExecCmd, ExecResult, Init, MAX_PAYLOAD, Message, Notification, Outcome,
    Progress, Read, ReadResult, RequestId, Server, Write, WriteResult,
};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;

use crate::contract::{GUEST_PATH, HANDSHAKE};
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
pub async fn run(port: std::fs::File, root: Option<PathBuf>) -> anyhow::Result<()> {
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
    let mut session = Session::default();

    while let Some(message) = server.recv().await? {
        match message {
            // The session is over. The host stops the guest right after this, so there is
            // nothing to release that the VM going away does not release.
            Message::Notification(Notification::Quit) => return Ok(()),

            // Booting early rather than under whichever call would have paid for it.
            // Nothing answers this, so a failure is only said here — the next call that
            // needs a boot tries again and tells whoever asked for it.
            Message::Notification(Notification::Start) => {
                if let Err(e) = session.booted() {
                    eprintln!("{}: linking delegated names: {e}", env!("CARGO_BIN_NAME"));
                }
            }

            Message::Notification(Notification::Stop) => session.release(),

            Message::Request { id, call } => match call {
                Call::Init(init) => {
                    session.configure(init);
                    server.respond(id, Outcome::Result(Bson::Null)).await?;
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

                Call::Exec(exec) => match session.booted() {
                    Ok(linked) => {
                        execute(&mut server, id, &exec, linked, &shims, root.as_deref()).await?
                    }
                    Err(e) => server.respond(id, boot_failed(e)).await?,
                },

                Call::Read(read) => {
                    let outcome = match session.booted() {
                        Ok(_) => read_file(&read).await,
                        Err(e) => boot_failed(e),
                    };
                    server.respond(id, outcome).await?;
                }

                Call::Write(write) => {
                    let outcome = match session.booted() {
                        Ok(_) => write_file(&write).await,
                        Err(e) => boot_failed(e),
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

/// What a session is in here: what the client announced, and whatever booting it took.
///
/// The two are apart because they are wanted at different moments. What [`Init`] carries
/// is the session's shape and costs nothing to hold; the directory of symlinks is a real
/// thing on the overlay, and the point of `start` and `stop` being optional is that it may
/// come and go underneath a session that does not change.
#[derive(Default)]
struct Session {
    /// What `init` said, or the defaults for a client that never sent one — which is a
    /// session with nothing delegated, and that is a session.
    config: Init,

    /// `None` until something boots it: a `start`, or the first call that needs one.
    linked: Option<BinDir>,
}

impl Session {
    /// Take a new shape, and let go of anything booted under the old one.
    ///
    /// The delegated names are built into the directory, so a boot from before this is a
    /// boot that no longer matches the session. Dropping it is enough — the next call
    /// that needs one builds it again, from what has just arrived.
    fn configure(&mut self, config: Init) {
        self.linked = None;
        self.config = config;
    }

    fn release(&mut self) {
        self.linked = None;
    }

    /// The delegated names somewhere `execvp` will find them, booting if nothing has.
    ///
    /// The directory is not put on `PATH`: every execution is given its own environment
    /// (see [`environment`]), which is what an inherited `PATH` and a `set_var` would
    /// otherwise be for — and doing it per command rather than per process is what lets
    /// this program have more than one thread.
    fn booted(&mut self) -> io::Result<&BinDir> {
        if self.linked.is_none() {
            self.linked = Some(BinDir::create(
                self.config.delegated.iter().map(String::as_str),
            )?);
        }
        Ok(self.linked.as_ref().expect("just booted"))
    }
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
/// # No `current_dir` here, and `root` is not for one
///
/// A command inherits this process's working directory, which
/// [`init::prepare`](crate::init::prepare) set to the workspace root; `read_file` and
/// `write_file` use their paths as given and resolve against the same one. **One setting, read
/// two ways** — and one setting cannot disagree with itself, which is why a command and a file
/// call name the same file here. `cortex-local-console` has to join an explicit root onto both
/// instead, because its process outlives many boots.
///
/// So do not add `.current_dir(root)`: a no-op today, and two settings that must stay equal
/// with nothing saying so. `root` is here for [`delegate`], which needs it as a value to strip.
async fn execute(
    server: &mut StdioServer,
    id: RequestId,
    exec: &Exec,
    linked: &BinDir,
    shims: &Shims,
    root: Option<&Path>,
) -> io::Result<()> {
    let Some((program, args)) = exec.cmd.split() else {
        return server
            .respond(id, refused(Error::INVALID_PARAMS, "an empty command"))
            .await;
    };

    let child = Command::new(program)
        .args(args)
        .envs(environment(linked, shims))
        // Piped and then read by `wait_with_output`, which is what carries the output
        // back. Input is at EOF from the start, since an `exec` carries none.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

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
                Some(stream) => match delegate(server, owed, stream, root).await? {
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
    root: Option<&Path>,
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

    // Workspace-relative, or `None` — the rule is `reported_cwd`'s, shared with the local
    // console.
    //
    // No canonicalization, deliberately: the root is `GUEST_WORKSPACE_ROOT`, a constant with
    // no symlink in it, in a filesystem the guest built. The local backend has to resolve its
    // mount point because macOS puts `$TMPDIR` under a `/var` symlink and reports the resolved
    // path. A root under a symlinked prefix would break here.
    let exec = Exec {
        cwd: cortex::executable::reported_cwd(exec.cwd.as_deref(), root),
        ..exec
    };

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

/// The environment variables an execution is given: the way home for a shim, and a `PATH`
/// with the delegated names on it.
///
/// `PATH` is composed rather than inherited. libkrun hands the guest's first process only
/// what the boot named, so there is no ambient `PATH` here to add to — see
/// [`GUEST_PATH`], which is what a login shell in this image would have had.
///
/// Per command rather than per process, for the same reason a host-local console does it
/// that way: `std::env::set_var` is unsound with any other thread running, and this
/// program has one.
fn environment(linked: &BinDir, shims: &Shims) -> Vec<(OsString, OsString)> {
    // Appended, not prepended: these names are meant to add commands, not to quietly
    // shadow a real `git` or `python` the image ships.
    let mut path = OsString::from(GUEST_PATH);
    path.push(":");
    path.push(linked.bin());

    vec![
        (SOCK_ENV.into(), shims.sock.clone().into_os_string()),
        ("PATH".into(), path),
    ]
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
    }))
}

/// Hand back part of a file, as the answer to the `read` that asked for it.
///
/// The size is taken before the bytes are, so a file that grows between the two is
/// reported as the shorter one it was — and one that shrinks is answered with however
/// much was still there. Either way `data` is what was read and `size` is what the file
/// measured, which is the pair a requester needs to know whether to ask again.
async fn read_file(read: &Read) -> Outcome {
    let path = Path::new(&read.path);

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
async fn write_file(write: &Write) -> Outcome {
    let path = Path::new(&write.path);

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
