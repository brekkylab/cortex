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
//! Building that directory is what booting is here, and [`Session`] is where it is kept.
//! It is built when something needs it and not when a client asks, so every request that
//! runs anything goes through [`Session::booted`] and none of them can find a session
//! that has not got one.
//!
//! `start` and `stop` are the client managing that resource rather than asking for it:
//! `start` builds the directory before a command has to wait for it, `stop` drops it so
//! nothing sits in `$TMPDIR` while the console is idle. Neither is answered and neither
//! changes what an `exec` can do.
//!
//! # Running the command, and pausing it
//!
//! [`Command`], captured — but spawned rather than run to completion, because a
//! delegated call arrives *while* it runs and has to be answered before it can finish.
//! So an execution is a [`select!`](tokio::select) over the two things that can happen
//! next: a shim connects, or the command ends.
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
//! A shim connecting means answering the console request with a
//! [`Delegated`](Progress::Delegated) and waiting for the `exec` that carries the answer
//! back — which is why the console channel is threaded through [`execute`] rather than
//! being answered once at the end. The client is still the only end that asks; this end
//! just answers more than once per execution, and each of those answers is to a request
//! of its own.
//!
//! Delegated calls are therefore served in turn. See [`Progress`] for why that is
//! latency rather than a deadlock.
//!
//! # Files
//!
//! `read` and `write` reach this host's filesystem directly, at the path resolved against
//! the session's namespace — the same directory a command runs in, so the two halves of
//! the protocol name the same file.
//!
//! That used to hold for free: with no per-command directory set, a spawned command and a
//! file call both inherited this process's cwd. Running commands *in* a mount removes that
//! reason, so the agreement is now something this module does — see [`at`] and
//! [`Booted::cwd`]. A session with no namespace still uses the path as it arrives.
//!
//! It is a namespace and not a confinement: an absolute path, or one with enough `..`,
//! still leaves the mount.
//!
//! Both are answered on the spot, outside any execution: nothing is spawned, nothing can
//! delegate, and one response ends it. They boot the session first because that rule is
//! the protocol's rather than this backend's — and here it is also what produces the
//! directory they resolve in.
//!
//! # One session at a time, on one task
//!
//! Everything above happens on the task that reads the channel, and nothing is answered
//! out of order: a request is taken, answered, and only then is the next one read. That is
//! the protocol rather than a shortcut — a client has one call outstanding at a time — and
//! it is what lets an execution own the channel for as long as its delegation chain runs.
//!
//! What concurrency there is sits underneath: a command runs while we wait, its output
//! drains while we answer a delegated call, and a shim's connection waits in the socket's
//! backlog until the execution that will serve it is ready.
//!
//! # What this does not do
//!
//! A delegated name is linked **without being checked as a plain path component** — so a
//! name like `../../etc/foo` would put a symlink somewhere this process does not reach
//! and cannot clean up. Every name that gets here came from the client, which is
//! in-process with whoever chose them; that is the only thing standing in for the check
//! today.
//!
//! A `read` or a `write` is not confined to anywhere either. The path is used as it
//! arrives, so a client can name any file this process can reach.
//!
//! Neither timeout is enforced. An `exec` carries a `timeout_ms` and an `init` carries
//! the `default_timeout_ms` to fall back on, and this server reads both and
//! applies neither — so a command that never ends is a command this server waits on
//! forever, and the client waits with it. What it takes is a bound around the `select!`
//! in [`execute`] and an answer for what a delegated call already in flight becomes when
//! it expires, which is the part that is a decision and not a line of code.

mod bin_dir;

use std::ffi::OsString;
use std::io::{self, SeekFrom};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};

use bson::Bson;
use cortex::CortexError;
use cortex::console::stdio::StdioServer;
use cortex::console::{
    Call, Error, Exec, ExecCmd, ExecResult, Init, MAX_PAYLOAD, Message, Notification, Outcome,
    Progress, Read, ReadResult, RequestId, Server, Write, WriteResult,
};
use cortex::volume::Workspace;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;

use crate::ipc::SOCK_ENV;
use bin_dir::SessionScratch;

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
pub async fn run() -> anyhow::Result<()> {
    let mut server = StdioServer::stdio()?;

    // Bound once, before anything can be running, and kept for the process. See
    // [`Shims::bind`] for why its lifetime is the process's and not a session's.
    let shims = Shims::bind()?;

    // Whatever it holds is dropped on `stop` and on the way out of this function
    // whichever way it leaves, which takes the symlinks with it every time.
    let mut session = Session::default();

    while let Some(message) = server.recv().await? {
        match message {
            // The session is over.
            Message::Notification(Notification::Quit) => return Ok(()),

            // Booting early rather than under whichever call would have paid for it.
            // Nothing answers this, so a failure is only said here — the next call that
            // needs a boot tries again and tells whoever asked for it.
            //
            // The message rather than the `Outcome`: the code belongs to whoever gets
            // answered, and nobody is being answered here. This line is for a person.
            Message::Notification(Notification::Start) => {
                if let Err(Outcome::Error(e)) = session.booted() {
                    eprintln!(
                        "{}: booting a session: {}",
                        env!("CARGO_BIN_NAME"),
                        e.message
                    );
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
                    Ok(booted) => execute(&mut server, id, &exec, booted, &shims).await?,
                    Err(outcome) => server.respond(id, outcome).await?,
                },

                // Booting first is the protocol's rule, and here it also decides *where*: a
                // file call resolves in the namespace booting produced.
                Call::Read(read) => {
                    let outcome = match session.booted() {
                        Ok(booted) => read_file(booted.cwd(), &read).await,
                        Err(outcome) => outcome,
                    };
                    server.respond(id, outcome).await?;
                }

                Call::Write(write) => {
                    let outcome = match session.booted() {
                        Ok(booted) => write_file(booted.cwd(), &write).await,
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

/// What a session is on this host: what the client announced, and whatever booting it
/// took.
///
/// The two are apart because they are wanted at different moments. What [`Init`] carries
/// is the session's shape and costs nothing to hold; the directory of symlinks is a real
/// thing on disk, and the point of `start` and `stop` being optional is that it may come
/// and go underneath a session that does not change.
#[derive(Default)]
struct Session {
    /// What `init` said, or the defaults for a client that never sent one — which is a
    /// session with nothing delegated, and that is a session.
    config: Init,

    /// `None` until something boots it: a `start`, or the first call that needs one.
    booted: Option<Booted>,
}

/// What a boot produced. Built together, released together.
struct Booted {
    /// Released before `scratch`, whose removal would otherwise block on a live mount.
    ///
    /// Field order is the backstop — `Drop` runs fields in declaration order — but
    /// [`Session::release`] does it explicitly, being the only path that *can* report a
    /// failure. `FuseTMount::unmount` has no fallible step, so the log below fires on a `fuse`
    /// build and never on a `fuse-t` one.
    #[cfg(any(feature = "fuse", feature = "fuse-t"))]
    mount: Option<cortex::volume::HostMount>,

    scratch: SessionScratch,
}

impl Booted {
    /// Where an execution runs, and where a file call resolves its path.
    ///
    /// The mount point when there is one, and otherwise whatever this process inherited —
    /// which is what a session with no declared namespace gets, exactly as before.
    fn cwd(&self) -> Option<&Path> {
        #[cfg(any(feature = "fuse", feature = "fuse-t"))]
        return self.mount.as_ref().map(|m| m.mountpoint());
        #[cfg(not(any(feature = "fuse", feature = "fuse-t")))]
        return None;
    }
}

impl Session {
    /// Take a new shape, and let go of anything booted under the old one.
    ///
    /// The delegated names are built into the directory, so a boot from before this is a
    /// boot that no longer matches the session. Releasing it is enough — the next call
    /// that needs one builds it again, from what has just arrived.
    ///
    /// Through [`release`](Self::release) rather than nulling the field: what a boot has
    /// to give back grows, and a re-`init` while a session is live has exactly as much
    /// reason to go through the one ordered teardown as a `stop` does.
    fn configure(&mut self, config: Init) {
        self.release();
        self.config = config;
    }

    /// Give back what booting took, mount first.
    ///
    /// Not left to field drop order: removing the scratch directory **blocks** on a live
    /// mount, and [`HostMount::unmount`](cortex::volume::HostMount) is the one path that
    /// can report a failure — `Drop` swallows it, because a `Drop` that panics mid-unwind
    /// aborts the process.
    ///
    /// # This can hang, and one of its callers is a call somebody is waiting on
    ///
    /// `unmount` joins the serving thread and that join has no timeout. Measured on a mac:
    /// five `FuseTMount`s torn down at once left one in uninterruptible sleep for 145
    /// seconds, out of reach of `kill`, until it was unmounted from outside by hand.
    ///
    /// Not reachable as things stand — one session per process, one mount per session — and
    /// whether the race exists *between* processes was never measured. What makes it worth
    /// writing down is that [`configure`](Self::configure) calls this, so a re-`init` over a
    /// wedged mount leaves a **request** unanswered where a `stop` could only cost silence.
    ///
    /// So: if a bounded teardown is ever needed, it belongs in `cortex`'s FUSE adapters
    /// rather than here, and both callers would need a way to give up.
    fn release(&mut self) {
        let Some(booted) = self.booted.take() else {
            return;
        };
        #[cfg(any(feature = "fuse", feature = "fuse-t"))]
        if let Some(mount) = booted.mount {
            // Nothing is waiting on this — `stop` is a notification and `init` answers
            // `null` — so a failure is said here and nowhere else, as a failed boot is.
            if let Err(e) = mount.unmount() {
                eprintln!("{}: unmounting the namespace: {e}", env!("CARGO_BIN_NAME"));
            }
        }

        // Without a mount binding there is nothing to give back in order, so releasing is
        // just letting go — said outright, because a binding this build does not have is
        // not a step it skips.
        #[cfg(not(any(feature = "fuse", feature = "fuse-t")))]
        drop(booted);

        // `scratch` drops here, with nothing mounted over it.
    }

    /// The session, booting if nothing has: the namespace realized, and the delegated
    /// names somewhere `execvp` will find them.
    ///
    /// The directory of symlinks is not put on `PATH`: every execution is given its own
    /// environment (see [`environment`]), which is what an inherited `PATH` and a
    /// `set_var` would otherwise be for — and doing it per command rather than per
    /// process is what lets this program have more than one thread.
    fn booted(&mut self) -> Result<&Booted, Outcome> {
        if self.booted.is_none() {
            // Cheapest first, and nothing is installed until all of it succeeded: a
            // failure here leaves the session unbooted, and the next call tries again —
            // which is the shape a client already has to handle.
            //
            // The workspace is not kept. Mounting moves it into the mount, which is the
            // only thing that reads it — nothing here asks a `Workspace` a question, and a
            // second handle would be state nobody looks at. What survives realization is
            // the mount, and where a build has none, what survives is the knowledge that
            // the spec was one this build could make sense of.
            let ws = Workspace::from_spec(&self.config.volumes).map_err(unsupported_volume)?;
            let scratch = SessionScratch::create(self.config.delegated.iter().map(String::as_str))
                .map_err(boot_failed)?;

            #[cfg(any(feature = "fuse", feature = "fuse-t"))]
            let mount = if self.config.volumes.is_empty() {
                // Nothing to make visible, and a mount is not free. A session that
                // declares no namespace pays for none.
                drop(ws);
                None
            } else {
                Some(cortex::volume::HostMount::spawn(ws, scratch.mnt()).map_err(mount_failed)?)
            };

            #[cfg(not(any(feature = "fuse", feature = "fuse-t")))]
            {
                drop(ws);
                // A namespace was declared and there is nothing here that could show it to
                // a command. The kinds were all fine, so this is about the build and not
                // the spec — see `MOUNT_FAILED`.
                if !self.config.volumes.is_empty() {
                    return Err(refused(
                        Error::MOUNT_FAILED,
                        "no mount binding compiled in: this build cannot make a namespace \
                         visible to a command",
                    ));
                }
            }

            self.booted = Some(Booted {
                #[cfg(any(feature = "fuse", feature = "fuse-t"))]
                mount,
                scratch,
            });
        }
        Ok(self.booted.as_ref().expect("just booted"))
    }
}

/// A boot that did not happen, as the answer to whatever needed one.
fn boot_failed(e: io::Error) -> Outcome {
    refused(Error::BOOT_FAILED, format!("linking delegated names: {e}"))
}

/// The path a file call names, in the namespace an execution would resolve it in.
///
/// A workspace path is relative and a mount point is where it starts, so this is a join —
/// the same one a spawned command's `current_dir` performs for it. With no mount there is
/// no directory to join, and the path is used as it arrives, which is what this backend did
/// before there were namespaces at all.
///
/// **A join, not containment.** An absolute path, or one with enough `..`, still leaves the
/// mount. What this buys is that the two halves of the protocol name the same file, not
/// that either half is confined.
fn at(root: Option<&Path>, path: &str) -> PathBuf {
    match root {
        Some(root) => root.join(path),
        None => PathBuf::from(path),
    }
}

/// A namespace that could not be bound to a filesystem interface. The kinds were fine.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
fn mount_failed(e: CortexError) -> Outcome {
    refused(Error::MOUNT_FAILED, format!("mounting the namespace: {e}"))
}

/// A volume kind this build cannot realize, as the answer to whatever needed a session.
fn unsupported_volume(e: CortexError) -> Outcome {
    let code = match e {
        CortexError::UnsupportedVolume(_) => Error::UNSUPPORTED_VOLUME,
        // Anything else from `from_spec` is a spec this server cannot make sense of — a
        // mount path that escapes the root, or two at one path.
        _ => Error::INVALID_PARAMS,
    };
    refused(code, format!("realizing the namespace: {e}"))
}

/// Run one command, answering the console channel as many times as it takes.
///
/// Exactly one of those answers is the execution's own — a [`Done`](Progress::Done) or an
/// error — and it goes to whichever request is owed one by then: the `exec` a caller
/// asked for if nothing was delegated, or the last `exec` that carried an answer back if
/// something was.
async fn execute(
    server: &mut StdioServer,
    id: RequestId,
    exec: &Exec,
    booted: &Booted,
    shims: &Shims,
) -> io::Result<()> {
    let Some((program, args)) = exec.cmd.split() else {
        return server
            .respond(id, refused(Error::INVALID_PARAMS, "an empty command"))
            .await;
    };

    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(environment(booted, shims))
        // Piped and then read by `wait_with_output`, which is what carries the output
        // back. Input is at EOF from the start, since an `exec` carries none.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // In the namespace, when there is one — which is what makes a relative path in a
    // command mean the same thing as one in a `read`.
    if let Some(dir) = booted.cwd() {
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
                Some(stream) => match delegate(server, owed, stream, booted.cwd()).await? {
                    Some(next) => owed = next,
                    // The client stopped saying anything that could carry the execution on,
                    // so there is nobody left to answer. The command is left to the
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

    // Workspace-relative, or `None`. The rule is `cortex`'s and not this backend's — see
    // `reported_cwd`, which the uvm guest agent calls for the same reason.
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
/// Per command rather than per process. `std::env::set_var` is unsound with any other
/// thread running and this program has one, so a `PATH` this process mutates once is a
/// `PATH` each [`Command`] is handed instead.
fn environment(booted: &Booted, shims: &Shims) -> Vec<(OsString, OsString)> {
    // Appended, not prepended: these names are meant to add commands, not to quietly
    // shadow a real `git` or `python` a caller meant to run.
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    path.push(":");
    path.push(booted.scratch.bin());

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
        // Under `/tmp` rather than `$TMPDIR`: `sockaddr_un.sun_path` is 104 bytes on
        // macOS and a per-user temp directory is most of that on its own.
        let sock = PathBuf::from(format!("/tmp/cortex-console-{}.sock", std::process::id()));
        // A live pid cannot have left one behind, so anything here is a reused pid whose
        // socket outlived its process.
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
