//! `cortex-local-console` — entry point for the host-local backend.
//!
//! Answers a console session on stdin and stdout: `init` to say which tree the session
//! works in, an `exec` per command, `quit` to end. See [`cortex::console`] for the shape
//! of what those descriptors carry.
//!
//! The tree is a `file://` URL and so a directory this host already has — the answer to
//! `init` is where it is, and every path afterwards is under it. Where the session
//! *stands* starts there and moves only through `cd`, which [`server`] answers itself
//! because it is a builtin rather than a program.
//!
//! Finding that directory is what booting is here, and nothing has to ask for it: the
//! first `exec`, `read` or `write` to find a session that has not booted is served by a
//! session that has just booted. `start` and `stop` are resource management on top of that
//! and nothing more — boot early so no command pays the cold start, release while idle so
//! nothing is held for a session that is doing nothing — which is why neither is answered
//! and neither is required. On this backend what they hand back and forth is almost
//! nothing, because a directory this host already has costs nothing to find; on one with a
//! guest to bring up it is not.

mod server;

use std::process::ExitCode;

/// A failure of ours, not the command's — the shell's code for "found it, could
/// not run it".
const NOT_EXECUTABLE: u8 = 126;

/// The server waits on things it does not drive — a pipe, a command — so it is async, and
/// this is the runtime under it.
///
/// Multi-threaded because a command's output drains while this end is still answering, and
/// because nothing about a session says how many threads the programs it spawns will want
/// out of the runtime they were spawned from.
#[tokio::main]
async fn main() -> ExitCode {
    // Each command's code travels back inside its own answer, so this one says only
    // whether the session itself worked.
    match server::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            ExitCode::from(NOT_EXECUTABLE)
        }
    }
}
