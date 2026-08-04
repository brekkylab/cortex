//! `cortex-local-console` — entry point for the host-local backend.
//!
//! Answers a console session on stdin and stdout: `start` to put the delegated names on
//! `PATH`, an `exec` per command, `stop` to take them away, `quit` to end. See
//! [`cortex::console`] for the shape of what those descriptors carry.
//!
//! Boot is a request of its own and comes before any command, so a client can start a
//! server before it knows what to run — and `start` being answered is proof the server is
//! ready.
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
//! name in `start` and one `symlink(2)`, and the whole thing deploys as the single file
//! cargo already produces.
//!
//! So a delegated `foo` plus a command of `sh -c 'foo | tr a-z A-Z'` prints `BAR`: `foo`
//! is not a file anyone built, it is a symlink to this binary, and what it printed came
//! from the client over a socket this process never touches.

mod ipc;
mod server;
mod shim;

use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;

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

fn main() -> ExitCode {
    match role() {
        // A shim exits with somebody else's code — the delegated executable's — which is
        // the whole point of it. A process can only exit 0..=255, so `shim` clamps.
        Role::Shim(name) => shim::run(&name),

        // A server does not. Each command's code travels back inside its own answer, so
        // this one only says whether the session itself worked.
        Role::Server => match server::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
                ExitCode::from(NOT_EXECUTABLE)
            }
        },
    }
}
