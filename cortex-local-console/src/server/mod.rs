//! The server role: answer a console session by running commands on this host.
//!
//! A [`StdioServer`] brings the requests in and puts the responses out, and does
//! nothing else — so what is left here is what is actually ours: making a delegated name
//! runnable, running a command, letting a delegated call reach the client while that
//! command waits for it, and reading and writing the files a command works with.
//!
//! # Making a delegated name runnable
//!
//! A `PATH` entry: a directory of symlinks, one per delegated name, each pointing back
//! at this binary (see [`bin_dir`]). A command that runs one of them re-enters this
//! binary as a shim, which dials the socket *we* bound — see [`ipc`](crate::ipc).
//!
//! Host-local by nature: a micro-VM backend can use neither a symlink on our filesystem
//! nor a socket on it.
//!
//! # Running the command, and pausing it
//!
//! [`Command`], captured — but spawned rather than run to completion, because a
//! delegated call arrives *while* it runs and has to be answered before it can finish.
//! So an execution is a loop over two things that can happen next: a shim connects, or
//! the command ends. Both arrive on one queue ([`Shims`]), because waiting for either is
//! what the loop does and there is no way to wait on two.
//!
//! A shim connecting means answering the console request with a
//! [`Delegated`](Progress::Delegated) and waiting for the client's `resume` — which is
//! why the console channel is threaded through [`execute`] rather than being answered
//! once at the end. The client is still the only end that asks; this end just answers
//! more than once per execution.
//!
//! Delegated calls are therefore served in turn. See [`Progress`] for why that is
//! latency rather than a deadlock.
//!
//! # Files
//!
//! `read` and `write` reach this host's filesystem directly, at the path as given — the
//! same namespace a command here resolves its own relative paths in, since a command here
//! is an ordinary process of this one's.
//!
//! Both are answered on the spot, outside any execution: nothing is spawned, nothing can
//! delegate, and one response ends it.
//!
//! # What this does not do
//!
//! The session rules are not enforced anywhere yet. An `exec` before `start` runs (with
//! no delegated names on `PATH`, since there are none), a second `start` re-boots, and a
//! delegated name is linked **without being checked as a plain path component** — so a
//! name like `../../etc/foo` would put a symlink somewhere this process does not reach
//! and cannot clean up. Every name that gets here came from the client, which is
//! in-process with whoever chose them; that is the only thing standing in for the check
//! today.
//!
//! A `read` or a `write` is not confined to anywhere either. The path is used as it
//! arrives, so a client can name any file this process can reach.

mod bin_dir;

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::FileExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};

use bson::Bson;
use cortex::console::stdio::StdioServer;
use cortex::console::{
    Call, Error, Exec, ExecResult, MAX_PAYLOAD, Message, Outcome, Progress, Read, ReadResult,
    RequestId, Server, Start, Write, WriteResult,
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

/// Answer requests until the client says `quit` or closes the channel.
pub fn run() -> anyhow::Result<()> {
    let mut server = StdioServer::stdio()?;

    // Bound once, before anything can be running, and kept for the process. See
    // [`Shims::bind`] for why its lifetime is the process's and not a session's.
    let shims = Shims::bind()?;

    // Dropped on `stop`, and on the way out of this function whichever way it leaves —
    // which takes the symlinks with it every time.
    let mut linked: Option<BinDir> = None;

    while let Some(message) = server.recv()? {
        match message {
            // The session is over.
            Message::Notification(_) => return Ok(()),

            Message::Request { id, call } => match call {
                Call::Start(start) => {
                    let outcome = boot(&mut linked, &start);
                    server.respond(id, outcome)?;
                }

                Call::Exec(exec) => execute(&mut server, id, &exec, linked.as_ref(), &shims)?,

                // Only an execution paused on a delegated call has anything to resume,
                // and one of those is answered inside `execute` — so reaching here is a
                // client resuming something that is not waiting.
                Call::Resume(_) => server.respond(
                    id,
                    refused(
                        Error::INVALID_REQUEST,
                        "nothing is waiting on a delegated call, so there is nothing to resume",
                    ),
                )?,

                Call::Read(read) => server.respond(id, read_file(&read))?,

                Call::Write(write) => server.respond(id, write_file(&write))?,

                Call::Stop => {
                    linked = None;
                    server.respond(id, Outcome::Result(Bson::Null))?;
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

/// Put the delegated names somewhere `execvp` will find them, as symlinks back to this
/// binary.
///
/// The directory is not put on `PATH` here. Every execution is given its own environment
/// (see [`environment`]), which is what an inherited `PATH` and a `set_var` used to be
/// for — and doing it per command rather than per process is what lets this program have
/// more than one thread.
fn boot(linked: &mut Option<BinDir>, start: &Start) -> Outcome {
    let dir = match BinDir::create(start.delegated.iter().map(String::as_str)) {
        Ok(dir) => dir,
        Err(e) => return refused(Error::BOOT_FAILED, format!("linking delegated names: {e}")),
    };

    // Any previous one drops here, which takes its symlinks with it.
    *linked = Some(dir);
    Outcome::Result(bson::Bson::Null)
}

/// Run one command, answering the console channel as many times as it takes.
///
/// Exactly one of those answers is the execution's own — a [`Done`](Progress::Done) or an
/// error — and it goes to whichever request is owed one by then: the `exec` if nothing was
/// delegated, or the last `resume` if something was.
fn execute(
    server: &mut StdioServer,
    id: RequestId,
    exec: &Exec,
    linked: Option<&BinDir>,
    shims: &Shims,
) -> io::Result<()> {
    let Some((program, args)) = exec.split() else {
        return server.respond(id, refused(Error::INVALID_PARAMS, "an empty command"));
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
            return server.respond(
                id,
                refused(
                    Error::NOT_EXECUTABLE,
                    format!("{program}: {e} (a shell would report {code})"),
                ),
            );
        }
    };

    // On the shims' queue rather than one of its own, so that this loop has a single
    // thing to wait on. `wait_with_output` reads both pipes as it waits, which is what
    // keeps a command that fills one from blocking on it.
    let ending = shims.sender();
    std::thread::spawn(move || {
        let _ = ending.send(Event::Exited(child.wait_with_output()));
    });

    // Which request this execution owes its answer to. The `exec` to begin with, and each
    // `resume` after that — a `Delegated` spends the one it is sent on.
    let mut owed = id;

    loop {
        match shims.next() {
            Event::Shim(stream) => match delegate(server, owed, stream)? {
                Some(next) => owed = next,
                // The client stopped saying anything that could resume the execution, so
                // there is nobody left to answer. The command is left to the process's
                // ending, which is moments away: the main loop reads the same channel.
                None => return Ok(()),
            },

            Event::Exited(output) => return server.respond(owed, finished(output)),
        }
    }
}

/// Hand one delegated call to the client, and give the shim what comes back.
///
/// Two messages on the console channel: the [`Delegated`](Progress::Delegated) that
/// answers what this execution currently owes, and the `resume` that brings the result.
/// What is returned is the id of the request now owed the execution's own answer, or
/// `None` when the client said nothing that could be one.
fn delegate(
    server: &mut StdioServer,
    owed: RequestId,
    stream: UnixStream,
) -> io::Result<Option<RequestId>> {
    let mut shim = StdioServer::new(stream.try_clone()?, stream);

    // A connection carrying anything but a shim's one `exec` is not something to forward,
    // and the execution still owes what it owed. Dropping the connection is all there is
    // to say to it.
    let (shim_id, exec) = match shim.recv()? {
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

    server.respond(owed, result(Progress::Delegated(exec)))?;

    // Only a `resume` can arrive now. This end owes an answer it has not sent, so there is
    // nothing else the client could be asking about.
    match server.recv()? {
        Some(Message::Request {
            id,
            call: Call::Resume(outcome),
        }) => {
            // Whatever the client said, verbatim: a refusal is as much an answer as a
            // result, and the shim is what turns either into an exit code.
            shim.respond(shim_id, outcome)?;
            Ok(Some(id))
        }

        // The shim goes unanswered on purpose. There is nothing to tell it, and a dropped
        // connection is what says so — the shim reports that on its own stderr.
        other => {
            if let Some(message) = other {
                eprintln!(
                    "{}: the client sent {message:?} instead of resuming",
                    env!("CARGO_BIN_NAME")
                );
            }
            Ok(None)
        }
    }
}

/// The environment variables an execution is given: the way home for a shim, and a `PATH`
/// with the delegated names on it.
///
/// Per command rather than per process. `std::env::set_var` is unsound with any other
/// thread running and this program now has one, so what used to be a `PATH` this process
/// mutated at `start` is a `PATH` each `Command` is handed.
fn environment(linked: Option<&BinDir>, shims: &Shims) -> Vec<(OsString, OsString)> {
    let mut env = vec![(SOCK_ENV.into(), shims.sock.clone().into_os_string())];

    if let Some(bin) = linked.map(BinDir::bin) {
        // Appended, not prepended: these names are meant to add commands, not to quietly
        // shadow a real `git` or `python` a caller meant to run.
        let mut path = std::env::var_os("PATH").unwrap_or_default();
        path.push(":");
        path.push(bin);
        env.push(("PATH".into(), path));
    }

    env
}

/// The socket every shim dials, and the queue of what an execution is waiting for.
///
/// # Why the process binds it and not a session
///
/// A thread blocked in `accept` cannot be told to stop, so there is one of them and it
/// lives as long as the process — which means the socket does too. Binding it per `start`
/// would leave a thread behind on every `stop`, each blocked on a socket nobody will ever
/// dial again.
///
/// The cost is a socket a server with nothing delegated never uses, and the window
/// between a `stop` and the next `start` in which a connection could queue up unserved.
struct Shims {
    /// Handed to every execution's environment, which is how a shim finds it.
    sock: PathBuf,

    /// Cloned per execution, so a command's ending queues where its shims do.
    ending: Sender<Event>,

    incoming: Receiver<Event>,
}

/// Something an execution is waiting for.
///
/// One enum and one queue because an execution waits for whichever comes first, and
/// `std`'s channels cannot be selected over.
enum Event {
    /// A delegated executable has run and is waiting to be told what it produced.
    Shim(UnixStream),

    /// The command itself is over, and this is everything it wrote.
    Exited(io::Result<Output>),
}

impl Shims {
    fn bind() -> anyhow::Result<Shims> {
        // Under `/tmp` rather than `$TMPDIR`: `sockaddr_un.sun_path` is 104 bytes on
        // macOS and a per-user temp directory is most of that on its own.
        let sock = PathBuf::from(format!("/tmp/cortex-console-{}.sock", std::process::id()));
        // A live pid cannot have left one behind, so anything here is a reused pid whose
        // socket outlived its process.
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)?;

        let (ending, incoming) = mpsc::channel();
        let accepting = ending.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let queued = match stream {
                    Ok(stream) => accepting.send(Event::Shim(stream)),
                    Err(e) => {
                        eprintln!("{}: no more shims: {e}", env!("CARGO_BIN_NAME"));
                        return;
                    }
                };
                // The receiver is held for the life of the process, so this only fails
                // once there is nothing left to serve.
                if queued.is_err() {
                    return;
                }
            }
        });

        Ok(Shims {
            sock,
            ending,
            incoming,
        })
    }

    fn sender(&self) -> Sender<Event> {
        self.ending.clone()
    }

    /// The next thing an execution is waiting for.
    ///
    /// Cannot fail: this struct holds a sender of its own, so the queue cannot be
    /// disconnected while it is alive.
    fn next(&self) -> Event {
        self.incoming
            .recv()
            .expect("a queue whose sender is held cannot disconnect")
    }
}

impl Drop for Shims {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock);
    }
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
fn read_file(read: &Read) -> Outcome {
    let path = Path::new(&read.path);

    let size = match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {
            return refused(
                Error::IS_A_DIRECTORY,
                format!("{}: is a directory", read.path),
            );
        }
        Ok(meta) => meta.len(),
        Err(e) => return file_error(e, &read.path),
    };

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => return file_error(e, &read.path),
    };

    // Starting past the end asks for nothing, which is an empty answer rather than an
    // error: `size` is there to say what the offset was past.
    let offset = read.offset.unwrap_or(0);
    let left = size.saturating_sub(offset);
    let want = read.len.unwrap_or(left).min(left).min(MAX_DATA) as usize;

    let mut data = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        match file.read_at(&mut data[filled..], offset + filled as u64) {
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
fn write_file(write: &Write) -> Outcome {
    let path = Path::new(&write.path);

    let file = match write.offset {
        None => std::fs::File::create(path),
        Some(_) => std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path),
    };

    let file = match file {
        Ok(file) => file,
        Err(e) => return file_error(e, &write.path),
    };

    if let Err(e) = file.write_all_at(&write.data, write.offset.unwrap_or(0)) {
        return file_error(e, &write.path);
    }

    match file.metadata() {
        Ok(meta) => encoded(bson::serialize_to_bson(&WriteResult { size: meta.len() })),
        Err(e) => file_error(e, &write.path),
    }
}

/// What went wrong with a file, as one of the codes a requester branches on.
///
/// Everything that is not the path being absent or being a directory is
/// [`IO_FAILED`](Error::IO_FAILED): permissions, a full disk, a name too long. The
/// message carries what the OS said, since that is the part worth reading.
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
