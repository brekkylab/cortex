use serde::{Deserialize, Serialize};

use crate::protocol::message::utils::bytes;

/// One execution. The `params` of `exec`.
///
/// Only a command and a time bound: where it runs is the session's, what it runs with is
/// the executor's, and its input is whatever a [`WriteCall`](super::WriteCall) staged beforehand.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecCall {
    /// The argv. No shell is involved; for shell semantics send `["sh", "-c", "..."]`.
    ///
    /// An empty argv is refused where the process is spawned, not when the call is read;
    /// see [`split`](Self::split).
    pub cmd: Vec<String>,

    /// Milliseconds before the executor kills it. `None` is no limit, and there is no
    /// session-wide default, so a never-ending command without one never ends.
    ///
    /// Expiry is an immediate kill with no grace period or second signal: a simple rule
    /// every backend (including a micro-VM guest) can keep. The requester gets
    /// [`TIMED_OUT`](crate::protocol::Error::TIMED_OUT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl ExecCall {
    /// The program and its arguments; `None` for an empty argv, which the spawner refuses
    /// as [`INVALID_PARAMS`](crate::protocol::Error::INVALID_PARAMS).
    pub fn split(&self) -> Option<(&String, &[String])> {
        self.cmd.split_first()
    }
}

/// A whole execution: everything it wrote, and how it ended. The `result` of `exec`.
///
/// An execution with no result (killed at its timeout, never started) is an `error` with
/// a code instead (see [`Error`](crate::protocol::Error)), since an exit code could be forged by `exit()`.
///
/// Sent unwrapped, so later additions are new members, which older peers ignore.
///
/// The session's current directory (see [`InitResp::cwd`](super::InitResp::cwd)) is not reported, even if the
/// command moved it: a result describes the command, not the machine. Run `pwd` to ask.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResp {
    /// For a command killed by a signal, `128 + signal` by convention.
    pub code: i32,

    /// Kept apart from [`stderr`](Self::stderr) because a requester can merge but not
    /// un-merge. Interleaving is not preserved; use `2>&1` if order matters.
    #[serde(with = "bytes")]
    pub stdout: Vec<u8>,

    #[serde(with = "bytes")]
    pub stderr: Vec<u8>,

    /// The command wrote more than the executor would hold, and the output here is its
    /// beginning.
    ///
    /// A result must fit one message under [`MAX_PAYLOAD`](crate::protocol::MAX_PAYLOAD).
    /// Flagged so an agent does not draw conclusions from output it thinks is complete.
    #[serde(default)]
    pub truncated: bool,
}
