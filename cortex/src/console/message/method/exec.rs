use serde::{Deserialize, Serialize};

use super::bytes;
use crate::console::{Outcome, RequestId};

/// One thing to run, and everything needed to run it. The `params` of `exec`, and
/// what a [`Progress::Delegated`] carries back the other way.
///
/// As `params` it is the command the session exists for. Inside a `Delegated` it is a
/// delegated executable the server cannot run itself, and `cmd[0]` is one of the
/// names that arrived in [`Initialize`].
///
/// One type for both because an execution request is an execution request no matter
/// who is asking whom: a command, output, a code at the end.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Exec {
    /// Already split into argv. Nothing here consults a shell, so quoting and word
    /// rules stay wherever the command was composed; a caller that wants shell
    /// semantics asks for them outright — `["sh", "-c", "..."]`.
    pub cmd: Vec<String>,

    /// The delegated call this execution carries on from, and `None` for one that is
    /// not carrying on from anything.
    ///
    /// It is what makes a paused execution resumable without a method of its own: a
    /// server that answered [`Progress::Delegated`] is holding an execution that owes
    /// output it cannot produce, and the `exec` carrying this is the client handing that
    /// output back. Which one it belongs to is [`PrevExecResult::id`], since a client
    /// that sent it is by definition sending a second request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<PrevExecResult>,

    /// How long this may run before the executor kills it, in milliseconds. `None`
    /// falls back to [`Initialize::default_timeout_ms`], and if that is `None` too
    /// there is no limit.
    ///
    /// Expiry is a kill: no grace period, no second signal, no negotiation. One
    /// rule is worth more here than a good one — a requester cannot reach into a
    /// micro-VM guest to check on anything, so anything subtler would be a promise
    /// only some backends could keep.
    ///
    /// The executor enforces it because only the executor knows what running the
    /// thing means. The requester hears [`TIMED_OUT`](super::Error::TIMED_OUT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// A whole execution in one value: everything it wrote, and how it ended. The
/// [`Done`](Progress::Done) of a `Progress`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
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
    /// A result travels in one message under [`MAX_PAYLOAD`](super::MAX_PAYLOAD), so
    /// a command that writes without limit has to be cut off somewhere. Saying so is
    /// the whole point of the field: an agent reading output it does not know is
    /// partial will draw a conclusion from it, and a wrong answer is worse than a
    /// short one.
    #[serde(default)]
    pub truncated: bool,
}

/// The delegated call this execution is carrying on from: which one it was, and how it
/// ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrevExecResult {
    /// The id of the request whose response asked for the delegated call, which is what
    /// says *which* one this answers.
    ///
    /// There is never more than one paused at a time, so it identifies nothing the
    /// server could not have worked out — what it earns is the same thing an `id` earns
    /// anywhere here: an answer carrying a number nobody is waiting on is a peer that
    /// has lost its place, and can be refused rather than mistaken for the one that was
    /// due.
    pub id: RequestId,

    /// An [`Outcome`] and not an [`ExecResult`], because a delegated call can fail to
    /// produce one at all: a name the client does not have is
    /// [`NOT_EXECUTABLE`](super::Error::NOT_EXECUTABLE) and one that ran too long is
    /// [`TIMED_OUT`](super::Error::TIMED_OUT), and the server hands whichever it gets
    /// straight to the shim that is waiting.
    ///
    /// An exit code could not say either of those. 127 for a program that was not there
    /// and 126 for one that could not be started are codes any command reaches by an
    /// ordinary `exit()`, so a result carrying one never proves the call failed rather
    /// than ran and failed — which is the whole reason this protocol's failures are
    /// codes on an `error` and not statuses on a result.
    pub outcome: Outcome,
}

/// How far an execution got: finished, or waiting for the client. The `result` of
/// `exec`.
///
/// # Why a result and not a request
///
/// A delegated executable's behaviour lives in the *client* — a Rust closure, an HTTP
/// call, whatever the host wants a tool to mean — so a command that invokes one by
/// name cannot be finished by the server alone. The server has to ask.
///
/// It asks by answering. [`Delegated`](Self::Delegated) is a complete, ordinary
/// response to the request the client is already waiting on, and it means *this
/// execution is not over and here is what I need from you*. The client runs the name and
/// sends another `exec` carrying the answer as its [`prev`](Exec::prev), whose response
/// is the next `Progress`. The chain ends at [`Done`](Self::Done).
///
/// So the server never issues a request and the client never answers one. Every channel
/// in the system has one end that only asks and one that only answers, which is what
/// there is to gain: no end needs a pending table, no end has a task waiting on
/// something only that task could read, and there is one channel rather than one per
/// delegated call.
///
/// # What it costs
///
/// **Delegated calls are served one at a time.** A response can carry one of these, so a
/// command that starts several delegated executables together (`foo & bar`, `make -j8`)
/// has them run in turn rather than at once. The [`timeout_ms`](Exec::timeout_ms) of the
/// outer execution has to cover the sum.
///
/// That is latency and not a deadlock, and only because delegated calls are independent
/// of each other: nothing an [`Exec`] carries is input, so no delegated call is waiting
/// on another being served first. **If input ever reaches a delegated call, serving them
/// in turn stops being safe** and this is the line of reasoning that has to change.
///
/// A client with no delegated names never sees anything but `Done`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Progress {
    /// The execution is over, and this is all of it.
    Done(ExecResult),

    /// The execution is paused on a delegated executable. Run it and send an [`Exec`]
    /// carrying the answer as its [`prev`](Exec::prev).
    ///
    /// Nothing else may be asked in between: the execution owes an answer that has not
    /// been sent, so a client that asks about anything else is asking about a request it
    /// has not been answered on.
    Delegated(Exec),
}

impl Exec {
    /// The program and its arguments, or `None` for an empty `cmd`.
    ///
    /// The split every executor needs, done once here rather than at each of them —
    /// and an empty command is refused rather than handed on as a program nobody can
    /// run.
    pub fn split(&self) -> Option<(&String, &[String])> {
        self.cmd.split_first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not utf-8, contains NUL and a newline: nothing about program bytes is special,
    /// and BSON carries them as themselves rather than as text.
    ///
    /// The assertion is on the *variant* and not only the round trip, because a round
    /// trip passes either way — base64 out and base64 back is symmetric, and would be
    /// 1.37× and the byte type unused with nothing to show it. That is the failure this
    /// test exists to catch; see [`bytes`](super::super::bytes) on why the codec is not
    /// asked.
    #[test]
    fn program_bytes_survive_as_bytes() {
        let bytes = vec![0xff, 0xfe, 0x00, b'\n', 0x00];
        let value = bson::serialize_to_bson(&ExecResult {
            stdout: bytes.clone(),
            stderr: bytes.clone(),
            ..ExecResult::default()
        })
        .unwrap();

        let doc = value.as_document().unwrap();
        assert_eq!(
            doc.get("stdout"),
            Some(&bson::Bson::Binary(bson::Binary {
                subtype: bson::spec::BinarySubtype::Generic,
                bytes: bytes.clone(),
            })),
        );

        let read: ExecResult = bson::deserialize_from_bson(value).unwrap();
        assert_eq!((read.stdout, read.stderr), (bytes.clone(), bytes));
    }

    /// An execution that sets no timeout and carries on from nothing is `cmd` alone:
    /// both members are absent rather than null, which is what `skip_serializing_if`
    /// buys — and both read back unset.
    #[test]
    fn an_unadorned_exec_is_a_command_and_nothing_else() {
        let exec = Exec {
            cmd: vec!["ls".into()],
            ..Exec::default()
        };
        let doc = bson::serialize_to_document(&exec).unwrap();
        assert_eq!(doc, bson::doc! {"cmd": ["ls"]});
        assert_eq!(bson::deserialize_from_document::<Exec>(doc).unwrap(), exec);
    }

    /// An empty command is refused here rather than handed on as a program nobody
    /// can run.
    #[test]
    fn an_exec_splits_into_a_program_and_its_arguments() {
        let exec = Exec {
            cmd: vec!["sh".into(), "-c".into(), "".into()],
            ..Exec::default()
        };
        let (program, args) = exec.split().unwrap();
        assert_eq!(program, "sh");
        assert_eq!(args, ["-c", ""]);

        assert!(Exec::default().split().is_none());
    }
}
