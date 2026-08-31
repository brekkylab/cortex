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
//! # The tree, and where the session stands in it
//!
//! A `file://` workfs is a directory this host already has, so **realizing one is checking
//! a claim rather than mounting anything**: `init` reads the URL and answers with the path
//! it names, and the boot is what has to find a directory there. Any other scheme is
//! [`UNSUPPORTED_WORKFS`](Error::UNSUPPORTED_WORKFS) at `init`, because whether this build
//! can realize a kind is knowable the moment the frame is read — see [`directory_url`].
//!
//! Where the session *stands* starts there and moves only through [`cd`](Session::change_dir),
//! which this server answers itself: it is a shell builtin rather than a program, so an
//! `exec` carrying an argv has no other way to offer it, and the shell a person would be
//! talking to is the session. A `cd` inside a command moves that command's own process and
//! nothing else, exactly as a subshell does in a terminal — so `sh -c 'cd x'` reports
//! nothing and the session is where it was.
//!
//! # Files
//!
//! `read` and `write` reach this host's filesystem directly, at the path resolved where the
//! session stands — the same directory a command runs in, so the two halves of the protocol
//! name the same file. See [`at`], which is the whole of that agreement.
//!
//! It is a place to stand and not a confinement: an absolute path, or one with enough `..`,
//! still leaves the tree.
//!
//! Both are answered on the spot, outside any execution: nothing is spawned, nothing can
//! delegate, and one response ends it. They boot the session first because that rule is
//! the protocol's rather than this backend's — and here it is also what checks that the
//! directory they resolve in is there at all.
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
//! **Only `cd` moves the session.** A command that changes its own directory some other way
//! — a program that `chdir`s, a shell script that ends somewhere else — is not followed,
//! because a child's working directory dies with the child and nothing here reads it back.
//! What that would take is a shell kept alive across executions, which is a different
//! backend rather than a line of code.
//!
//! Neither timeout is enforced. An `exec` carries a `timeout_ms` and an `init` carries
//! the `default_timeout_ms` to fall back on, and this server reads both and
//! applies neither — so a command that never ends is a command this server waits on
//! forever, and the client waits with it. What it takes is a bound around the `select!`
//! in [`execute`] and an answer for what a delegated call already in flight becomes when
//! it expires, which is the part that is a decision and not a line of code.

mod bin_dir;

use std::{
    ffi::OsString,
    io::{self, SeekFrom},
    os::unix::process::ExitStatusExt as _,
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Output, Stdio},
};

use bin_dir::SessionScratch;
use bson::Bson;
use cortex::console::{
    Call, Error, Exec, ExecCmd, ExecResult, ImageSource, Init, InitResult, MAX_PAYLOAD, Message,
    NetworkAccess, Notification, Outcome, Progress, Read, ReadResult, RequestId, Server,
    WorkFsMount, WorkFsSource, Write, WriteResult, stdio::StdioServer,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _},
    net::{UnixListener, UnixStream},
    process::Command,
};

use crate::ipc::SOCK_ENV;

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
                // A session is taken or it is not: a workfs this server cannot put
                // anywhere is refused rather than answered with a path, since a path is
                // what every later call the client makes would be spelled in.
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

                // Booting first is the protocol's rule, and here it also decides *where*: a
                // relative path lands where the session stands, which is where a command
                // would have looked for it.
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

/// What a session is on this host: what the client announced, where it stands, and
/// whatever booting it took.
///
/// The three are apart because they are wanted at different moments. What [`Init`] carries
/// is the session's shape and costs nothing to hold; where it stands is a path and costs no
/// more; the directory of symlinks is a real thing on disk, and the point of `start` and
/// `stop` being optional is that it may come and go underneath a session that does not
/// change.
#[derive(Default)]
struct Session {
    /// What `init` said, or the defaults for a client that never sent one — which is a
    /// session with nothing delegated, and that is a session.
    config: Init,

    /// The directory a `file://` workfs named, or `None` for a session that has none.
    ///
    /// Kept apart from [`cwd`](Self::cwd) because they answer different questions and stop
    /// being the same path the first time a command runs `cd`: this one is the tree, and it
    /// is what `cd` with no argument goes back to.
    workfs: Option<PathBuf>,

    /// Where the session stands, which is what an execution runs in and what a relative
    /// path in a file call is resolved against.
    ///
    /// The workfs when there is one, this process's own directory when there is not, and
    /// `None` when even that could not be read — a session that then names nothing and
    /// leaves every path as it arrives, which is what this backend did before it had a tree
    /// to stand in.
    ///
    /// **Not part of what booting produced**, so a `stop` does not disturb it: a `PathBuf`
    /// is not occupancy, and a client that ran a `cd`, went idle and came back should not
    /// find itself somewhere it never asked to be.
    cwd: Option<PathBuf>,

    /// `None` until something boots it: a `start`, or the first call that needs one.
    booted: Option<Booted>,
}

/// What a boot produced. Built together, released together.
struct Booted {
    scratch: SessionScratch,

    /// The tree as `getcwd(2)` will spell it — symlinks resolved — or `None` for a session
    /// with no tree.
    ///
    /// Taken at boot because that is where the directory is looked at anyway, and it cannot
    /// be taken any earlier: `init` answers a path before anything has been asked to find
    /// one there. What it is for is [`respelled`], which is the only reader.
    physical: Option<PathBuf>,
}

impl Session {
    /// Take a new shape, answering what the client has to know about it.
    ///
    /// The URL is read **before** anything is let go of, so a session this server cannot
    /// take is one it has not taken: a client whose second `init` is refused still has the
    /// one it had.
    ///
    /// Otherwise the old boot goes, because the delegated names are built into the
    /// directory and a boot from before this is a boot that no longer matches the session.
    /// Releasing it is enough — the next call that needs one builds it again, from what has
    /// just arrived.
    fn configure(&mut self, config: Init) -> Result<InitResult, Outcome> {
        let workfs = config.workfs.as_ref().map(directory_url).transpose()?;
        base(config.image.as_ref())?;
        reach(config.network.as_ref())?;

        // Through `release` rather than nulling the field: what a boot has to give back
        // grows, and a re-`init` while a session is live has exactly as much reason to go
        // through the one ordered teardown as a `stop` does.
        self.release();

        // A session with no tree still stands somewhere — this process's own directory,
        // which is where a spawned command would have run anyway. Saying so is better than
        // leaving the client to guess what a relative path would mean.
        self.cwd = workfs.clone().or_else(|| std::env::current_dir().ok());
        self.workfs = workfs;
        self.config = config;

        Ok(InitResult {
            workfs: self.workfs.as_deref().map(|path| WorkFsMount {
                // The URL it came from was a `String`, so this one round-trips.
                path: path.to_string_lossy().into_owned(),
            }),
            // A directory with no `String` form is one the client could not have used, so
            // it is left unsaid rather than sent lossily — the rule a `cwd` follows
            // everywhere in this protocol.
            cwd: self.named_cwd(),
            // Nothing to name. A command here runs on this host's own filesystem, which is
            // not an image and has no reference — see [`base`].
            image: None,
            // Whatever this host reaches, which is the only answer this backend has — see
            // [`reach`].
            network: Some(NetworkAccess::full()),
        })
    }

    /// Where the session stands.
    fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// The same, as the protocol can carry it: `None` for a directory with no `String`
    /// form, which is a name the client could not have joined onto anyway.
    fn named_cwd(&self) -> Option<String> {
        self.cwd().and_then(|cwd| cwd.to_str().map(str::to_owned))
    }

    /// The tree under both its spellings — what `init` answered, and what `getcwd(2)` will
    /// say — for [`respelled`]. `None` before a boot and for a session with no tree.
    fn tree(&self) -> Option<(&Path, &Path)> {
        let physical = self.booted.as_ref()?.physical.as_deref()?;
        Some((self.workfs.as_deref()?, physical))
    }

    /// What booting produced, for the one thing outside this type that needs it.
    ///
    /// Reached separately from [`boot`](Self::boot) rather than returned by it, because a
    /// `cd` between two commands needs the session back — and a borrow handed out by the
    /// call that booted would still be held while it asked for that.
    fn scratch(&self) -> Option<&SessionScratch> {
        self.booted.as_ref().map(|booted| &booted.scratch)
    }

    /// Give back what booting took.
    fn release(&mut self) {
        // `scratch` drops with it, which is the whole of it: nothing is mounted over the
        // tree, because a `file://` one is a directory this host already had.
        self.booted = None;
    }

    /// Bring the session up if nothing has: the tree where `init` said it would be, and the
    /// delegated names somewhere `execvp` will find them.
    ///
    /// Realizing a `file://` workfs is **checking a claim rather than mounting anything** —
    /// the directory is one this host already has — and it happens here rather than at
    /// `init` because that is where the protocol puts it: `init` says where the tree will
    /// be, and a boot is what has to find it there. A directory that is not there is the
    /// environment being wrong for a session that is described correctly, which is what
    /// [`MOUNT_FAILED`](Error::MOUNT_FAILED) says.
    ///
    /// The directory of symlinks is not put on `PATH`: every execution is given its own
    /// environment (see [`environment`]), which is what an inherited `PATH` and a `set_var`
    /// would otherwise be for — and doing it per command rather than per process is what lets
    /// this program have more than one thread.
    fn boot(&mut self) -> Result<(), Outcome> {
        if self.booted.is_some() {
            return Ok(());
        }

        // Canonicalizing is the check: it fails for a directory that is not there, and what
        // it produces is the spelling a shim will report from inside the tree.
        let physical = match &self.workfs {
            None => None,
            Some(workfs) => match workfs.canonicalize() {
                Ok(physical) if physical.is_dir() => Some(physical),
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
            },
        };

        let scratch = SessionScratch::create(self.config.delegated.iter().map(String::as_str))
            .map_err(boot_failed)?;
        self.booted = Some(Booted { scratch, physical });
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
    /// the builtin it stands in for, so a directory that is not there is a non-zero code and
    /// a line on stderr rather than the session refusing the call.
    ///
    /// **Logical, like a shell's own `cd`.** The path is normalized on paper — `.` dropped,
    /// `..` popped — and symlinks are left alone, so what goes back is still spelled under
    /// the path `init` answered with. Canonicalizing instead would be `cd -P`, and it breaks
    /// the one thing these paths are for: a mount point reached through a symlink is
    /// answered as `/var/…` at `init` and would come back as `/private/var/…` here, which
    /// the client cannot relate to what it was told. See [`respelled`].
    ///
    /// The directory still has to be there — the normalization is about spelling, and this
    /// stats what it produced.
    fn change_dir(&mut self, argv: &[String]) -> Outcome {
        let target = match argv {
            // `cd` with nothing is the tree it started in, which is this session's spelling
            // of what `$HOME` is to a shell.
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
        // A directory the client cannot be told about is one it must not be moved into: the
        // paths it built afterwards would name files nobody meant.
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

/// `path` with `.` dropped and `..` popped, on paper and without touching the filesystem.
///
/// What a shell does to keep a working directory readable, and here it also keeps it
/// *comparable*: every path this server reports has to be spelled under the one `init`
/// answered with, and resolving symlinks would produce a second spelling of the same
/// directory that the client has no way to relate to the first.
///
/// A `..` popped on paper can name a different directory than a kernel would reach through
/// a symlinked component. That is the same trade a shell's logical `cd` makes, and the
/// alternative here is worse: a path the other end cannot use at all.
///
/// `..` at the root stays at the root, the way `cd /..` does.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            component => out.push(component),
        }
    }
    out
}

/// A directory a shim reported, spelled the way this session spells things.
///
/// `getcwd(2)` answers the *physical* path — symlinks resolved, always — so a shim standing
/// in a tree mounted at `/var/…` reports `/private/var/…`, which is not a path the client
/// was ever told about. Only this end can put that right: it knows both spellings, and the
/// one the client can use is the one `init` answered with.
///
/// So a directory inside the tree comes back re-rooted onto that answer, and one outside it
/// is passed through as it stands — a path this session never described, which the client
/// will decline to resolve anything against rather than guess at.
///
/// `None` in and `None` out: a shim that reported nothing leaves the client refusing
/// relative arguments, which is the honest end of it.
fn respelled(reported: Option<String>, tree: Option<(&Path, &Path)>) -> Option<String> {
    let reported = reported?;
    let Some((answered, physical)) = tree else {
        return Some(reported);
    };
    match Path::new(&reported).strip_prefix(physical) {
        Ok(rest) => answered.join(rest).to_str().map(str::to_owned),
        Err(_) => Some(reported),
    }
}

/// The argv of an `exec` that is a `cd`, without the name — or `None` for anything else.
///
/// The whole of what makes it a builtin: matched on the first word, before a process is
/// spawned. Nothing else about an `exec` is read differently.
fn cd_target(exec: &Exec) -> Option<&[String]> {
    match exec.cmd.split() {
        Some((program, args)) if program == "cd" => Some(args),
        _ => None,
    }
}

/// The directory a workfs URL names, or why it names none this server can use.
///
/// `file://` and nothing else, which is the whole of what this build realizes — and the
/// scheme is where that is decided, not the path, so anything else is
/// [`UNSUPPORTED_WORKFS`](Error::UNSUPPORTED_WORKFS) naming what was asked for.
///
/// Reading the URL is [`WorkFsSource`]'s, so that this server and any other realize the same
/// string the same way. What is left here is the two refusals, which are this build's: a
/// scheme it has no provider for, and a path that is not absolute — `file://srv/x`, whose
/// authority is not something this can honour and whose path two ends would resolve
/// A base a session named, refused, because there is nothing here to be one.
///
/// A command on this backend is a process on this host, running against the filesystem this
/// server can see. There is no root to swap and no overlay to put one under, so an image is not
/// a narrower session this could give: it is a different backend.
///
/// Which makes this the second place in this build where a *capability* decides a refusal
/// rather than a provider, the first being the reach a session asks for. Asking for nothing is
/// not asking for less: an `init` with no image gets the session it always got.
fn base(asked: Option<&ImageSource>) -> Result<(), Outcome> {
    let Some(asked) = asked else {
        return Ok(());
    };

    Err(refused(
        Error::UNSUPPORTED_IMAGE,
        format!(
            "{}: commands here run on this host's own filesystem, so there is no base to put \
             under them — an image needs a backend that runs them somewhere else",
            asked.reference
        ),
    ))
}

/// differently.
/// Refuse any reach but `full`, which is the only one a host-local session has.
///
/// A command here is a process on this host, sharing this host's network with everything else
/// on it. There is no device to leave off and no policy to enforce — so `none`, `host` and
/// `public` are not narrower sessions this backend could offer, they are promises it cannot
/// keep, and the honest answer is
/// [`UNSUPPORTED_NETWORK`](Error::UNSUPPORTED_NETWORK) rather than a session that reaches
/// everything after being asked for less.
///
/// Which makes this the one place in this build where a *capability* decides a refusal rather
/// than a provider. A client that needs the narrower thing needs a backend whose commands run
/// somewhere it can be taken away — `cortex-uvm-console`, whose guest has an interface only if
/// one was attached.
///
/// Asking for nothing is not asking for less: an `init` with no network gets the session it
/// always got, answered with what it actually has.
fn reach(asked: Option<&NetworkAccess>) -> Result<(), Outcome> {
    let Some(asked) = asked else {
        return Ok(());
    };
    if asked.reach == NetworkAccess::full().reach {
        return Ok(());
    }

    Err(refused(
        Error::UNSUPPORTED_NETWORK,
        format!(
            "{}: commands here run on this host and share its network, so only `full` can be \
             answered — a narrower reach needs a backend that runs them somewhere else",
            asked.reach
        ),
    ))
}

fn directory_url(workfs: &WorkFsSource) -> Result<PathBuf, Outcome> {
    let Some(path) = workfs.file_path() else {
        return Err(refused(
            Error::UNSUPPORTED_WORKFS,
            format!(
                "{}: this server realizes file:// and nothing else",
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

/// The file a path names, resolved where the session stands.
///
/// The protocol's paths are this host's, so an absolute one is already the answer and
/// `Path::join` says so by dropping the root it was given. A relative one lands where a
/// command would have looked for it, which is the join a spawned command's `current_dir`
/// performs for itself. With nowhere to stand there is nothing to join, and the path is
/// used as it arrives.
///
/// **A join, not containment.** An absolute path, or one with enough `..`, still leaves the
/// tree. What this buys is that the two halves of the protocol name the same file, not that
/// either half is confined.
fn at(root: Option<&Path>, path: &str) -> PathBuf {
    match root {
        Some(root) => root.join(path),
        None => PathBuf::from(path),
    }
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

    // Where the session stands, which is what makes a relative path in a command mean the
    // same thing as one in a `read` — and what a `cd` before this one moved.
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
                Some(stream) => match delegate(server, owed, stream, session.tree()).await? {
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
    tree: Option<(&Path, &Path)>,
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

    // The environment goes verbatim — it is the command's own, and nothing about it is
    // this end's to interpret. The directory is re-spelled, because `getcwd(2)` answers a
    // physical path and the client was told about a possibly different spelling of the same
    // tree. See [`respelled`].
    let exec = Exec {
        cwd: respelled(exec.cwd, tree),
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
fn environment(session: &Session, shims: &Shims) -> Vec<(OsString, OsString)> {
    // Appended, not prepended: these names are meant to add commands, not to quietly
    // shadow a real `git` or `python` a caller meant to run.
    let mut path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(scratch) = session.scratch() {
        path.push(":");
        path.push(scratch.bin());
    }

    let mut env = vec![
        (SOCK_ENV.into(), shims.sock.clone().into_os_string()),
        ("PATH".into(), path),
    ];

    // `PWD` is what a shell reads to answer `pwd`, and it is inherited from this process
    // unless something says otherwise — so a command spawned in the session's directory
    // would be told it was standing somewhere else. Set to match, which is what a shell
    // does for itself when it moves.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A URL is refused for the reason it actually has, and the two reasons are different
    /// answers: a scheme this build has no provider for is the *build* being wrong for a
    /// well-formed request, where a path that is not absolute is the request.
    ///
    /// Neither is reachable through a `Console`, which names a mount point it holds and so
    /// can only ever write `file://` and an absolute path — which is exactly why they are
    /// asserted here.
    #[test]
    fn a_workfs_url_is_a_file_url_or_it_is_refused() {
        assert_eq!(
            directory_url(&WorkFsSource::new("file:///srv/project")).unwrap(),
            PathBuf::from("/srv/project")
        );

        for (url, code, because) in [
            (
                "https://example.com/share",
                Error::UNSUPPORTED_WORKFS,
                "https",
            ),
            ("s3://bucket/prefix", Error::UNSUPPORTED_WORKFS, "s3"),
            // No scheme at all is not a path this server may fall back to reading: it is a
            // URL that names no kind, and the message says the whole of what arrived.
            ("/srv/project", Error::UNSUPPORTED_WORKFS, "/srv/project"),
            // An authority is not something this can honour, so what follows `file://` has
            // to be the path itself.
            ("file://srv/project", Error::INVALID_PARAMS, "absolute"),
        ] {
            let Err(Outcome::Error(e)) = directory_url(&WorkFsSource::new(url)) else {
                panic!("{url} was accepted");
            };
            assert_eq!(e.code, code, "{url}: {}", e.message);
            assert!(e.message.contains(because), "{url}: {}", e.message);
        }
    }

    /// `cd` is the first word and nothing else about an `exec` is read differently — not a
    /// command that merely mentions it, and not an answer to a delegated call.
    #[test]
    fn cd_is_the_first_word_of_a_command_and_nothing_else() {
        let argv = |words: &[&str]| Exec {
            cmd: ExecCmd::New(words.iter().map(|w| w.to_string()).collect()),
            ..Exec::default()
        };

        assert_eq!(cd_target(&argv(&["cd"])), Some(&[][..]));
        assert_eq!(
            cd_target(&argv(&["cd", "work"])),
            Some(&["work".to_string()][..])
        );
        assert!(cd_target(&argv(&["sh", "-c", "cd work"])).is_none());
        assert!(cd_target(&argv(&["cdx"])).is_none());
        assert!(cd_target(&argv(&[])).is_none());
        assert!(
            cd_target(&Exec {
                cmd: ExecCmd::Resume {
                    id: 0,
                    outcome: Outcome::Result(Bson::Null),
                },
                ..Exec::default()
            })
            .is_none()
        );
    }
}
