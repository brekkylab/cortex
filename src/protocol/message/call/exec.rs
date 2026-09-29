use serde::{Deserialize, Serialize};

use crate::protocol::message::utils::bytes;

/// One execution, and everything it needs. The `params` of `exec`.
///
/// A command and a bound on how long it may take, and nothing else — because nothing else
/// about an execution is the client's to say. Where it runs is the session's, which the far
/// end keeps; what it runs with is the executor's; what it reads is whatever a
/// [`WriteCall`] put where it would look.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecCall {
    /// Run this. Already split into argv, and nothing here consults a shell, so quoting
    /// and word rules stay wherever the command was composed; a caller that wants shell
    /// semantics asks for them outright — `["sh", "-c", "..."]`.
    ///
    /// An empty argv is a command nobody can run. It is refused where a command is turned
    /// into a process rather than everywhere an `exec` is read, because that is the end
    /// that knows what running one means — see [`split`](Self::split).
    pub cmd: Vec<String>,

    /// How long this may run before the executor kills it, in milliseconds. `None` is
    /// no limit: an [`InitCall`] carries no default to fall back on, so a command that
    /// never ends and was given no timeout is one nobody ends.
    ///
    /// Expiry is a kill: no grace period, no second signal, no negotiation. One
    /// rule is worth more here than a good one — a requester cannot reach into a
    /// micro-VM guest to check on anything, so anything subtler would be a promise
    /// only some backends could keep.
    ///
    /// The executor enforces it because only the executor knows what running the
    /// thing means. The requester hears [`TIMED_OUT`](crate::console::Error::TIMED_OUT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl ExecCall {
    /// The program and its arguments.
    ///
    /// The split every executor needs, done once here rather than at each of them — and
    /// `None` for an empty argv, which is a command that was never usable and is refused
    /// as [`INVALID_PARAMS`](crate::console::Error::INVALID_PARAMS) by whoever would have spawned
    /// it.
    pub fn split(&self) -> Option<(&String, &[String])> {
        self.cmd.split_first()
    }
}

/// A whole execution in one value: everything it wrote, and how it ended. The `result` of
/// `exec`.
///
/// A failure is not one of these. An execution that produced no result at all — killed at
/// its timeout, or never started — travels as an `error` with a code on it, see
/// [`Error`]: a code an ordinary `exit()` can reach could never have proved the difference.
///
/// It goes on the wire unwrapped, so what a peer adds to this later is a *member*. That is
/// the extension this protocol already handles — an unknown member is ignored everywhere —
/// where a tagged alternative beside it would be a shape an older peer could only fail on.
///
/// # What is not here: where the session ended up
///
/// A session has a current directory and an execution can move it — see
/// [`InitResp::cwd`] — and none of that is reported back on this.
///
/// Because a shell does not report it either. A terminal answers `cd work` with nothing at
/// all, and a person who wants to know where they are types `pwd`; a client is in exactly
/// that position, and `pwd` is an `exec` like any other. A member repeated on every result
/// to say "unmoved" for nearly every one of them is a member a reader stops looking at, and
/// the one time it matters is the one time it can be asked for.
///
/// What that costs is a round trip, on the executions where a client actually needs the
/// answer. What it buys is that a result describes the *command* — what it wrote, how it
/// ended — and nothing about the machine it ran on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResp {
    /// A command killed by a signal has no code of its own; the convention is
    /// `128 + signal`.
    pub code: i32,

    /// Kept apart from [`stderr`](Self::stderr) because merging is something a
    /// requester can do and un-merging is not: a value the caller wanted only the
    /// data from, and a diagnostic it wanted to show or log, are two different
    /// things once they have arrived. The interleaving between them is not
    /// preserved — two buffers are not one stream — and a caller that needs the
    /// order asks the command for it (`2>&1`).
    #[serde(with = "bytes")]
    pub stdout: Vec<u8>,

    #[serde(with = "bytes")]
    pub stderr: Vec<u8>,

    /// Whether the command wrote more than the executor was willing to hold, and
    /// what is here is the beginning of it.
    ///
    /// A result travels in one message under [`MAX_PAYLOAD`](crate::console::MAX_PAYLOAD), so
    /// a command that writes without limit has to be cut off somewhere. Saying so is
    /// the whole point of the field: an agent reading output it does not know is
    /// partial will draw a conclusion from it, and a wrong answer is worse than a
    /// short one.
    #[serde(default)]
    pub truncated: bool,
}
