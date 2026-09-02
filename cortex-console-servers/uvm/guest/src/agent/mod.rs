//! The agent role: answer a console session by running commands in this guest.
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
//! Neither timeout is enforced. An `exec` carries a `timeout_ms` and an `init` carries
//! the fallback, and this agent reads both and applies neither — so a command that never
//! ends is one this agent waits on forever, and the client waits with it.
//!
//! [`cortex-local-console`]: https://docs.rs/cortex-local-console

use std::ffi::OsString;
use std::io::{self, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};

use cortex::console::stdio::StdioServer;
use cortex::console::{
    Call, Error, ExecCall, ExecResp, InitCall, InitResp, MAX_PAYLOAD, Message, Notification,
    ReadCall, ReadResp, Response, Server, WorkFsMount, WorkFsSource, WriteCall, WriteResp,
};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::process::Command;

use crate::contract::{GUEST_PATH, HANDSHAKE, ImageSpec};

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

/// What a session is in here: the tree it works in, where it stands in it, the root it
/// runs on, and whether it has booted.
///
/// The [`InitCall`] that produced it is not kept: everything in it that this end acts on is
/// [`workfs`](Self::workfs), and the base and the reach are answered one boot up by the
/// server that started this guest — see [`configure`](Self::configure).
///
/// **The same shape the host-local backend has**, deliberately: a client cannot tell which
/// one answered it, so what a session *is* must not depend on which one did. What differs
/// between them is only where a command runs, which is the whole of what a backend is for.
#[derive(Default)]
struct Session {
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
    /// one it has not taken. Otherwise the old boot goes: the tree is what a boot checked
    /// for, and a boot from before this is a boot that no longer matches.
    fn configure(&mut self, config: InitCall) -> Result<InitResp, Error> {
        let workfs = config.workfs.as_ref().map(directory_url).transpose()?;

        self.release();
        // A session with no tree stands where this process was put, which `init::prepare`
        // set to `/`. Saying so beats leaving the client to guess what a relative path means.
        self.cwd = workfs.clone().or_else(|| std::env::current_dir().ok());
        self.workfs = workfs;

        Ok(InitResp {
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

/// The directory a workfs URL names, or why it names none this agent can use.
///
/// `file://` and nothing else. Inside a guest that is not a limitation the way it is on the
/// host: whatever the tree is made of was realized before the VM started, and what reaches
/// here is the directory it was mounted at.
fn directory_url(workfs: &WorkFsSource) -> Result<PathBuf, Error> {
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
        // Piped and then read by `wait_with_output`, which is what carries the output
        // back. Input is at EOF from the start, since an `exec` carries none.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

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

    // `PWD` is what a shell reads to answer `pwd`, and a command spawned in the session's
    // directory would otherwise be told it was standing wherever this process is. Set to
    // match, which is what a shell does for itself when it moves.
    if let Some(cwd) = session.cwd() {
        env.push(("PWD".into(), cwd.into()));
    }
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

    result(ExecResp {
        code: exit_code(&out.status),
        stdout: out.stdout,
        stderr: out.stderr,
        truncated: false,
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
