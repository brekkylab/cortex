//! The server: answer a console session by running commands on this host.
//!
//! A [`StdioServer`] brings the requests in and puts the responses out, and does nothing
//! else — so what is left here is what is actually ours: running a command, keeping track
//! of where the session stands, and reading and writing the files a command works with.
//!
//! # Running the command
//!
//! [`Command`], captured: spawned, both pipes drained, and one response when it ends. An
//! execution is one request and one answer, so there is nothing to interleave and nothing
//! to hold — which is what makes this end a function of the request rather than a state
//! machine with an execution in it.
//!
//! Booting is what has to happen before that: this backend's is finding the directory
//! `init` named, and it is done when something needs it rather than when a client asks, so
//! every request that runs anything goes through [`Session::boot`] and none of them can
//! find a session that has not booted.
//!
//! `start` and `stop` are the client managing that rather than asking for it, and on this
//! backend they are nearly free either way. They are answered so that a client written
//! against a backend where they are *not* free — one with a kernel to bring up — talks to
//! this one without changing.
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
//! nothing else, exactly as a subshell does in a terminal — so `sh -c 'cd x'` leaves the
//! session where it was.
//!
//! Nothing is reported about it either way. A `cd` is answered with a code and no output,
//! the way a terminal answers one, and a client that wants to know where it is runs `pwd`
//! — which lands here as an ordinary command, in the directory this session is holding.
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
//! Both boot the session first, because that rule is the protocol's rather than this
//! backend's — and here it is also what checks that the directory they resolve in is there
//! at all.
//!
//! # One session at a time, on one task
//!
//! Everything above happens on the task that reads the channel, and nothing is answered
//! out of order: a request is taken, answered, and only then is the next one read. That is
//! the protocol rather than a shortcut — a client has one call outstanding at a time.
//!
//! What concurrency there is sits underneath: a command's two pipes drain while this end
//! waits for it to end.
//!
//! # What this does not do
//!
//! A `read` or a `write` is not confined to anywhere. The path is used as it arrives, so a
//! client can name any file this process can reach.
//!
//! **Only `cd` moves the session.** A command that changes its own directory some other way
//! — a program that `chdir`s, a shell script that ends somewhere else — is not followed,
//! because a child's working directory dies with the child and nothing here reads it back.
//! What that would take is a shell kept alive across executions, which is a different
//! backend rather than a line of code.
//!
//! Neither timeout is enforced. An `exec` carries a `timeout_ms` and an `init` carries
//! the `default_timeout_ms` to fall back on, and this server reads both and applies
//! neither — so a command that never ends is a command this server waits on forever, and
//! the client waits with it.

use std::{
    ffi::OsString,
    io::{self, SeekFrom},
    os::unix::process::ExitStatusExt as _,
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Output, Stdio},
};

use cortex::console::{
    Call, Error, ExecCall, ExecResp, ImageSource, InitCall, InitResp, MAX_PAYLOAD, Message,
    NetworkAccess, Notification, ReadCall, ReadResp, Response, Server, WorkFsMount, WorkFsSource,
    WriteCall, WriteResp, stdio::StdioServer,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _},
    process::Command,
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

/// Answer requests until the client says `quit` or closes the channel.
pub async fn run() -> anyhow::Result<()> {
    let mut server = StdioServer::stdio()?;
    let mut session = Session::default();

    while let Some(message) = server.recv().await? {
        match message {
            // The session is over.
            Message::Notification(Notification::Quit) => return Ok(()),

            // Booting early rather than under whichever call would have paid for it.
            // Nothing answers this, so a failure is only said here — the next call that
            // needs a boot tries again and tells whoever asked for it.
            //
            // The message rather than the `Error`: the code belongs to whoever gets
            // answered, and nobody is being answered here. This line is for a person.
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
                // A session is taken or it is not: a workfs this server cannot put
                // anywhere is refused rather than answered with a path, since a path is
                // what every later call the client makes would be spelled in.
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

                // Booting first is the protocol's rule, and here it also decides *where*: a
                // relative path lands where the session stands, which is where a command
                // would have looked for it.
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

                // Nothing to keep. Commands here run on this host's own filesystem, so a
                // session writes into whatever was already there rather than over a base —
                // there is no difference to hand back, and pretending otherwise would mean
                // committing the host. The same refusal an image gets, for the same reason.
                Call::Commit(_) => {
                    let refusal = refused(
                        Error::UNSUPPORTED_IMAGE,
                        "commands here run on this host's own filesystem, so a session writes \
                         over nothing and has nothing to keep — committing needs a backend that \
                         runs them somewhere else",
                    );
                    server.respond(id, Response::Error(refusal)).await?;
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

/// What a session is on this host: the tree it works in, where it stands in it, and
/// whether it has booted.
///
/// The [`InitCall`] that produced it is not kept, because on this backend there is nothing left
/// of it to keep: the tree becomes [`workfs`](Self::workfs), and the base and the reach are
/// checked at `init` and refused there — a session that exists is one that asked for what
/// this backend has, so holding the request would be holding an answer already given.
#[derive(Default)]
struct Session {
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

    /// Whether the directory `init` named has been found where it said it would be.
    ///
    /// A flag and not a resource, because on this backend booting *is* the check: a
    /// `file://` workfs is a directory this host already has, so there is nothing to bring
    /// up and nothing to hand back. What it earns is that the check happens once per boot
    /// rather than once per request, and that `stop` means the same thing here as on a
    /// backend where it costs something.
    booted: bool,
}

impl Session {
    /// Take a new shape, answering what the client has to know about it.
    ///
    /// The URL is read **before** anything is let go of, so a session this server cannot
    /// take is one it has not taken: a client whose second `init` is refused still has the
    /// one it had.
    ///
    /// Otherwise the old boot goes, because the tree is what a boot checked for and a boot
    /// from before this is a boot that no longer matches the session. Releasing it is
    /// enough — the next call that needs one boots again, against what has just arrived.
    fn configure(&mut self, config: InitCall) -> Result<InitResp, Error> {
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

        Ok(InitResp {
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

    /// Give back what booting took.
    ///
    /// Which is nothing at all here — a `file://` tree is a directory this host already had
    /// and nothing was mounted over it — so this is the session forgetting that it checked.
    /// It is still not a no-op: the next call that needs a session checks again, which is
    /// what a client that sent `stop` because the tree might go away is asking for.
    fn release(&mut self) {
        self.booted = false;
    }

    /// Bring the session up if nothing has: the tree where `init` said it would be.
    ///
    /// Realizing a `file://` workfs is **checking a claim rather than mounting anything** —
    /// the directory is one this host already has — and it happens here rather than at
    /// `init` because that is where the protocol puts it: `init` says where the tree will
    /// be, and a boot is what has to find it there. A directory that is not there is the
    /// environment being wrong for a session that is described correctly, which is what
    /// [`MOUNT_FAILED`](Error::MOUNT_FAILED) says.
    fn boot(&mut self) -> Result<(), Error> {
        if self.booted {
            return Ok(());
        }

        // Canonicalizing is the check: it fails for a directory that is not there, and
        // asking for the resolved path is what makes the failure specific rather than a
        // `stat` that could have been about anything on the way down.
        if let Some(workfs) = &self.workfs {
            match workfs.canonicalize() {
                Ok(physical) if physical.is_dir() => {}
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
    /// the builtin it stands in for, so a directory that is not there is a non-zero code and
    /// a line on stderr rather than the session refusing the call.
    ///
    /// **Logical, like a shell's own `cd`.** The path is normalized on paper — `.` dropped,
    /// `..` popped — and symlinks are left alone, so where the session lands is still spelled
    /// under the path `init` answered with. Canonicalizing instead would be `cd -P`, and it
    /// breaks the one thing these paths are for: a mount point reached through a symlink is
    /// answered as `/var/…` at `init` and would turn into `/private/var/…` here — so the
    /// `PWD` a later command is handed, and the `pwd` a client reads out of it, would be a
    /// path it was never told about.
    ///
    /// The directory still has to be there — the normalization is about spelling, and this
    /// stats what it produced.
    fn change_dir(&mut self, argv: &[String]) -> Response {
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
        // A directory with no `String` form is one this protocol cannot spell at all, so it
        // is one the session must not stand in: a `read` resolved against it would name a
        // file nobody meant, and nothing could say which.
        if moved.to_str().is_none() {
            return builtin_failed(format!(
                "cd: {}: has no name this protocol can carry",
                moved.display()
            ));
        }

        self.cwd = Some(moved);
        // Nothing on the result says where that was. A shell answers `cd` with nothing
        // either, and a client that wants to know runs `pwd` — see
        // [`ExecResp`](cortex::console::ExecResp).
        result(ExecResp {
            code: 0,
            ..ExecResp::default()
        })
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

/// The argv of an `exec` that is a `cd`, without the name — or `None` for anything else.
///
/// The whole of what makes it a builtin: matched on the first word, before a process is
/// spawned. Nothing else about an `exec` is read differently.
fn cd_target(exec: &ExecCall) -> Option<&[String]> {
    match exec.split() {
        Some((program, args)) if program == "cd" => Some(args),
        _ => None,
    }
}

/// A base a session named, refused, because there is nothing here to be one.
///
/// A command on this backend is a process on this host, running against the filesystem this
/// server can see. There is no root to swap and no overlay to put one under, so an image is not
/// a narrower session this could give: it is a different backend.
///
/// Which makes this the second place in this build where a *capability* decides a refusal
/// rather than a provider, the first being the reach a session asks for. Asking for nothing is
/// not asking for less: an `init` with no image gets the session it always got.
fn base(asked: Option<&ImageSource>) -> Result<(), Error> {
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
fn reach(asked: Option<&NetworkAccess>) -> Result<(), Error> {
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
/// differently.
fn directory_url(workfs: &WorkFsSource) -> Result<PathBuf, Error> {
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
fn builtin_failed(message: impl Into<String>) -> Response {
    let mut stderr: Vec<u8> = message.into().into_bytes();
    stderr.push(b'\n');
    result(ExecResp {
        code: 1,
        stderr,
        ..ExecResp::default()
    })
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

/// Run one command, and answer with everything it produced.
///
/// One request, one answer: the command is spawned, both of its pipes are drained, and
/// what comes back is how it ended. Nothing arrives on the channel in between, which is
/// why this is a function of the request rather than something threaded through the
/// server.
async fn execute(exec: &ExecCall, session: &Session) -> Response {
    let Some((program, args)) = exec.split() else {
        return Response::Error(refused(Error::INVALID_PARAMS, "an empty command"));
    };

    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(environment(session))
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

    let child = match cmd.spawn() {
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

    // `wait_with_output` is what keeps both pipes draining while the command runs: a
    // command that fills a pipe nobody is reading stops there, and neither of these is
    // read anywhere else.
    finished(child.wait_with_output().await)
}

/// The environment variables an execution is given, on top of the ones it inherits.
///
/// Per command rather than per process. `std::env::set_var` is unsound with any other
/// thread running and this program has one, so a variable this process would have mutated
/// once is one each [`Command`] is handed instead.
fn environment(session: &Session) -> Vec<(OsString, OsString)> {
    // `PWD` is what a shell reads to answer `pwd`, and it is inherited from this process
    // unless something says otherwise — so a command spawned in the session's directory
    // would be told it was standing somewhere else. Set to match, which is what a shell
    // does for itself when it moves.
    //
    // The one variable here, because it is the one thing about this process's environment
    // that a spawned command would otherwise be wrong about: everything else it inherits
    // is as true for it as it is for us.
    match session.cwd() {
        Some(cwd) => vec![("PWD".into(), cwd.into())],
        None => Vec::new(),
    }
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
async fn read_file(root: Option<&Path>, read: &ReadCall) -> Response {
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
async fn write_file(root: Option<&Path>, write: &WriteCall) -> Response {
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
/// [`IO_FAILED`](Error::IO_FAILED): permissions, a full disk, a name too long. The
/// message carries what the OS said, since that is the part worth reading.
fn file_error(e: io::Error, path: &str) -> Error {
    let code = match e.kind() {
        io::ErrorKind::NotFound => Error::NOT_FOUND,
        io::ErrorKind::IsADirectory => Error::IS_A_DIRECTORY,
        _ => Error::IO_FAILED,
    };
    refused(code, format!("{path}: {e}"))
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
            let Err(e) = directory_url(&WorkFsSource::new(url)) else {
                panic!("{url} was accepted");
            };
            assert_eq!(e.code, code, "{url}: {}", e.message);
            assert!(e.message.contains(because), "{url}: {}", e.message);
        }
    }

    /// `cd` is the first word and nothing else about an `exec` is read differently — not a
    /// command that merely mentions it, and not one whose name only begins with it.
    #[test]
    fn cd_is_the_first_word_of_a_command_and_nothing_else() {
        let argv = |words: &[&str]| ExecCall {
            cmd: words.iter().map(|w| w.to_string()).collect(),
            ..ExecCall::default()
        };

        assert_eq!(cd_target(&argv(&["cd"])), Some(&[][..]));
        assert_eq!(
            cd_target(&argv(&["cd", "work"])),
            Some(&["work".to_string()][..])
        );
        assert!(cd_target(&argv(&["sh", "-c", "cd work"])).is_none());
        assert!(cd_target(&argv(&["cdx"])).is_none());
        assert!(cd_target(&argv(&[])).is_none());
    }
}
