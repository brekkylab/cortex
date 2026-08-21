//! `cortex-local-console` — entry point for the host-local backend.
//!
//! Answers a console session on stdin and stdout: `init` to say which names the session
//! delegates and which tree it works in, an `exec` per command, `quit` to end. See
//! [`cortex::console`] for the shape of what those descriptors carry.
//!
//! The tree is a `file://` URL and so a directory this host already has — the answer to
//! `init` is where it is, and every path afterwards is under it. Where the session
//! *stands* starts there and moves only through `cd`, which [`server`] answers itself
//! because it is a builtin rather than a program.
//!
//! Putting the delegated names on `PATH` is booting, and nothing has to ask for it: the
//! first `exec`, `read` or `write` to find a session without one is served by a session
//! that has just booted. `start` and `stop` are resource management on top of that and
//! nothing more — boot early so no command pays the cold start, release while idle so
//! the directory is not sitting there — which is why neither is answered and neither is
//! required. On this backend what they hand back and forth is a directory of symlinks,
//! which is cheap; on one with a guest to bring up it is not.
//!
//! # Two roles, one binary
//!
//! Booting puts the delegated executables on `PATH`, as symlinks back to this same
//! binary. So this program is reached two ways, and [`role`] tells them apart by the name
//! it was invoked under:
//!
//! - under its own name — **server**: answer the session (see [`server`]).
//! - under any other — **shim**: that name is a delegated executable someone just ran, so
//!   report the call over the socket, put the answer on our own descriptors, and exit with
//!   its code (see [`shim`]).
//!
//! There is nothing to build or ship for the second role. A delegated executable costs one
//! name in `init` and one `symlink(2)`, and the whole thing deploys as the single
//! file cargo already produces.
//!
//! So a delegated `foo` plus a command of `sh -c 'foo | tr a-z A-Z'` prints `BAR`: `foo`
//! is not a file anyone built, it is a symlink to this binary, and what it printed came
//! from the client — which the server asked, by answering the `exec` it was still working
//! on with a `Delegated` instead of a result.

mod ipc;
mod server;
mod shim;

use std::{ffi::OsStr, path::Path, process::ExitCode};

/// A failure of ours, not the command's — the shell's code for "found it, could
/// not run it".
const NOT_EXECUTABLE: u8 = 126;

/// Which of the two jobs this process was started to do.
enum Role {
    /// Run the command we were given.
    Server,
    /// Stand in for the named delegated executable.
    Shim(String),
}

/// Decide the role from `argv[0]`.
///
/// `execvp` puts the path it resolved into `argv[0]`, so a call through one of our
/// symlinks arrives carrying the link's name — which is the executable's name, and
/// the only thing distinguishing that invocation from a normal one. Comparing
/// against `CARGO_BIN_NAME` rather than a literal keeps this correct if the crate
/// is renamed again.
fn role() -> Role {
    let argv0 = std::env::args().next().unwrap_or_default();
    match Path::new(&argv0).file_name().and_then(OsStr::to_str) {
        Some(name) if name != env!("CARGO_BIN_NAME") => Role::Shim(name.to_string()),
        // No `argv[0]` at all: nothing claims we are a shim, so serve.
        _ => Role::Server,
    }
}

/// Both roles wait on something they do not drive — a socket, a pipe, a command — so
/// both are async, and one runtime serves either.
///
/// Multi-threaded because the server's is: an execution waits on its command and on the
/// shims that command dials, and a delegated call is answered while the command that made
/// it is still running. A shim is one round trip and would not have noticed either way.
#[tokio::main]
async fn main() -> ExitCode {
    match role() {
        // A shim exits with somebody else's code — the delegated executable's — which is
        // the whole point of it. A process can only exit 0..=255, so `shim` clamps.
        Role::Shim(name) => shim::run(&name).await,

        // A server does not. Each command's code travels back inside its own answer, so
        // this one only says whether the session itself worked.
        Role::Server => match server::run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
                ExitCode::from(NOT_EXECUTABLE)
            }
        },
    }
}
