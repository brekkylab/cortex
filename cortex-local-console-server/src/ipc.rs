//! The wire between a shim and the server that put it on `PATH`.
//!
//! A second protocol, distinct from the stdio one in [`cortex::console`]: that
//! one carries "run this argv" inward from whoever launched us, this one carries
//! "I was called, here is how" back from a process we ourselves caused to exist.
//! Same framing — one JSON object per line, one reply per request — because the
//! reasons for it are the same, and a shim that speaks the loop it already knows
//! needs no second parser.
//!
//! The transport is a unix socket rather than the shim's own stdio: its stdio
//! belongs to whatever ran it, which may be a pipeline, and is the only place
//! the output can go.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where the shim finds the socket. Exported into every child's environment by
/// the server at boot, so anything spawned under it — including a shell, and
/// anything that shell spawns — inherits the way home.
pub const SOCK_ENV: &str = "CORTEX_CONSOLE_SOCK";

/// A shim reporting the call it was made with.
#[derive(Serialize, Deserialize, Debug)]
pub struct Call {
    /// The name it was invoked by, taken from `argv[0]` — which is the symlink
    /// `execvp` resolved, not this binary's own name.
    pub name: String,

    /// Everything after `argv[0]`.
    pub args: Vec<String>,

    /// The shim's working directory, so a relative path in `args` means what it
    /// meant to the caller. The server's own cwd is unrelated to the caller's.
    pub cwd: PathBuf,
}

/// What the executable did, for the shim to replay as its own.
#[derive(Serialize, Deserialize, Debug)]
pub struct CallReply {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}
