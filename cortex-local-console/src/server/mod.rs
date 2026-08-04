//! The server role: answer a console session by running commands on this host.
//!
//! A [`StdioResponder`] brings the requests in and puts the responses out, and does nothing
//! else — so what is left here is the two things that are actually ours: making a
//! delegated name runnable, and running a command.
//!
//! # Making a delegated name runnable
//!
//! A `PATH` entry: a directory of symlinks, one per delegated name, each pointing back
//! at this binary (see [`bin_dir`]). A command that runs one of them re-enters this
//! binary as a shim, which reaches the *client* directly — see [`ipc`](crate::ipc) for
//! why that is not our hop to make. Nothing here answers a delegated call, and nothing
//! here knows what one does.
//!
//! Host-local by nature: a micro-VM backend can use neither a symlink on our filesystem
//! nor a socket on it.
//!
//! # Running the command
//!
//! [`Command`], captured. Everything a command wrote comes back inside one
//! [`ExecResult`], because that is the shape of the protocol's answer — there are no
//! output chunks and no way to watch a command work.
//!
//! # What this does not do
//!
//! The session rules are not enforced anywhere yet. An `exec` before `start` runs, a
//! second `start` re-boots, and a delegated name is linked **without being checked as a
//! plain path component** — so a name like `../../etc/foo` would put a symlink somewhere
//! this process does not reach and cannot clean up. Every name that gets here came from
//! the client, which is in-process with whoever chose them; that is the only thing
//! standing in for the check today.

mod bin_dir;

use std::os::unix::process::ExitStatusExt as _;
use std::process::{Command, ExitStatus};

use cortex::console::stdio::StdioResponder;
use cortex::console::{Call, Error, Exec, ExecResult, Message, Outcome, Responsable, Start};

use bin_dir::BinDir;

/// A command we found but could not start, and one we could not find at all.
///
/// Both are codes an ordinary command can reach by its own `exit()`, which is why neither
/// goes back *as* a code: they travel as an [`Error`], which cannot be mistaken for
/// something a command chose.
const NOT_EXECUTABLE: i32 = 126;
const NOT_FOUND: i32 = 127;

/// Answer requests until the client says `quit` or closes the channel.
pub fn run() -> anyhow::Result<()> {
    let mut server = StdioResponder::stdio()?;

    // Dropped on `stop`, and on the way out of this function whichever way it leaves —
    // which takes the symlinks with it every time.
    let mut linked: Option<BinDir> = None;

    while let Some(message) = server.recv()? {
        match message {
            // The session is over.
            Message::Notification(_) => return Ok(()),

            Message::Request { id, call } => {
                let outcome = match call {
                    Call::Start(start) => boot(&mut linked, &start),
                    Call::Exec(exec) => run_one(&exec),
                    Call::Stop => {
                        linked = None;
                        Outcome::Result(serde_json::Value::Null)
                    }
                };
                server.respond(id, outcome)?;
            }

            // A response answers a request, and this end makes none.
            Message::Response { id, .. } => eprintln!(
                "{}: a response arrived for request {id}, which nobody made",
                env!("CARGO_BIN_NAME")
            ),
        }
    }
    Ok(())
}

/// Put the delegated names on `PATH`, as symlinks back to this binary.
///
/// Nothing is done about the socket a shim will dial: the client bound it and put it in
/// the environment we were started with, so everything we spawn inherits it already.
fn boot(linked: &mut Option<BinDir>, start: &Start) -> Outcome {
    let dir = match BinDir::create(start.delegated.iter().map(String::as_str)) {
        Ok(dir) => dir,
        Err(e) => return refused(Error::BOOT_FAILED, format!("linking delegated names: {e}")),
    };

    // SAFETY: this process is single-threaded — the console channel is read and written
    // on this thread and there is no other — so nothing can be reading the environment
    // concurrently.
    unsafe {
        // Appended, not prepended: these names are meant to add commands, not to quietly
        // shadow a real `git` or `python` a caller meant to run.
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{path}:{}", dir.bin().display()));
    }

    // Any previous one drops here, after the new `PATH` is already in place.
    *linked = Some(dir);
    Outcome::Result(serde_json::Value::Null)
}

/// Run one command and answer with everything it produced.
fn run_one(exec: &Exec) -> Outcome {
    let Some((program, args)) = exec.split() else {
        return refused(Error::INVALID_PARAMS, "an empty command");
    };

    let out = match Command::new(program).args(args).output() {
        Ok(out) => out,
        Err(e) => {
            let code = match e.kind() {
                std::io::ErrorKind::NotFound => NOT_FOUND,
                _ => NOT_EXECUTABLE,
            };
            return refused(
                Error::NOT_EXECUTABLE,
                format!("{program}: {e} (a shell would report {code})"),
            );
        }
    };

    let result = ExecResult {
        code: exit_code(&out.status),
        stdout: out.stdout,
        stderr: out.stderr,
        truncated: false,
    };
    match serde_json::to_value(result) {
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
