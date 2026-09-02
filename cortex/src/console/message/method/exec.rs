use serde::{Deserialize, Serialize};

use super::bytes;

/// One execution, and everything it needs. The `params` of `exec`.
///
/// A command and a bound on how long it may take, and nothing else — because nothing else
/// about an execution is the client's to say. Where it runs is the session's, which the far
/// end keeps; what it runs with is the executor's; what it reads is whatever a
/// [`WriteCall`](super::WriteCall) put where it would look.
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

    /// How long this may run before the executor kills it, in milliseconds. `None`
    /// falls back to the default the session was announced with in [`InitCall`](super::InitCall),
    /// and if there is none there is no limit.
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

impl ExecCall {
    /// The program and its arguments.
    ///
    /// The split every executor needs, done once here rather than at each of them — and
    /// `None` for an empty argv, which is a command that was never usable and is refused
    /// as [`INVALID_PARAMS`](super::Error::INVALID_PARAMS) by whoever would have spawned
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
/// [`Error`](super::Error): a code an ordinary `exit()` can reach could never have proved
/// the difference.
///
/// It goes on the wire unwrapped, so what a peer adds to this later is a *member*. That is
/// the extension this protocol already handles — an unknown member is ignored everywhere —
/// where a tagged alternative beside it would be a shape an older peer could only fail on.
///
/// # What is not here: where the session ended up
///
/// A session has a current directory and an execution can move it — see
/// [`InitResp::cwd`](super::InitResp::cwd) — and none of that is reported back on this.
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
    /// A result travels in one message under [`MAX_PAYLOAD`](super::MAX_PAYLOAD), so
    /// a command that writes without limit has to be cut off somewhere. Saying so is
    /// the whole point of the field: an agent reading output it does not know is
    /// partial will draw a conclusion from it, and a wrong answer is worse than a
    /// short one.
    #[serde(default)]
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use bson::{Bson, Document, doc};

    use super::*;

    /// ReadCall an `exec` off the bytes a document makes, which is what a peer would actually
    /// have sent.
    fn read(doc: Document) -> Result<ExecCall, bson::error::Error> {
        bson::deserialize_from_slice(&bson::serialize_to_vec(&doc).unwrap())
    }

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
        let value = bson::serialize_to_bson(&ExecResp {
            stdout: bytes.clone(),
            stderr: bytes.clone(),
            ..ExecResp::default()
        })
        .unwrap();

        let doc = value.as_document().unwrap();
        assert_eq!(
            doc.get("stdout"),
            Some(&Bson::Binary(bson::Binary {
                subtype: bson::spec::BinarySubtype::Generic,
                bytes: bytes.clone(),
            })),
        );

        let read: ExecResp = bson::deserialize_from_bson(value).unwrap();
        assert_eq!((read.stdout, read.stderr), (bytes.clone(), bytes));
    }

    /// An execution that sets no timeout is a `cmd` and nothing else: the member is absent
    /// rather than null, which is what `skip_serializing_if` buys — and it reads back
    /// unset.
    #[test]
    fn an_unadorned_exec_is_a_command_and_nothing_else() {
        let exec = ExecCall {
            cmd: vec!["ls".into()],
            ..ExecCall::default()
        };
        let doc = bson::serialize_to_document(&exec).unwrap();
        assert_eq!(doc, doc! {"cmd": ["ls"]});
        assert_eq!(read(doc).unwrap(), exec);

        let bounded = ExecCall {
            timeout_ms: Some(5_000),
            ..exec
        };
        let doc = bson::serialize_to_document(&bounded).unwrap();
        assert_eq!(doc, doc! {"cmd": ["ls"], "timeout_ms": 5_000i64});
        assert_eq!(read(doc).unwrap(), bounded);
    }

    /// An unknown member is ignored here as it is everywhere else in this protocol, so a
    /// peer may add one.
    #[test]
    fn an_unknown_member_is_ignored() {
        assert_eq!(
            read(doc! {"cmd": ["ls"], "took_ms": 3i64}).unwrap().cmd,
            vec!["ls".to_string()],
        );
    }

    /// An empty command is refused where a command becomes a process, which is why it is
    /// a `None` here rather than a rejection at the frame.
    #[test]
    fn a_command_splits_into_a_program_and_its_arguments() {
        let exec = ExecCall {
            cmd: vec!["sh".into(), "-c".into(), "".into()],
            ..ExecCall::default()
        };
        let (program, args) = exec.split().unwrap();
        assert_eq!(program, "sh");
        assert_eq!(args, ["-c", ""]);

        assert!(ExecCall::default().split().is_none());
    }

    /// The `result` of an `exec` is the ending itself, with nothing around it — so what a
    /// peer adds to it later is a member, which every reader here already ignores when it
    /// does not know it.
    #[test]
    fn an_ending_is_the_result_with_nothing_around_it() {
        let ended = ExecResp {
            code: 0,
            stdout: b"hi\n".to_vec(),
            ..ExecResp::default()
        };
        let doc = bson::serialize_to_document(&ended).unwrap();
        assert!(doc.contains_key("code"), "{doc:?}");
        assert_eq!(
            bson::deserialize_from_document::<ExecResp>(doc).unwrap(),
            ended
        );

        // A member nobody here has heard of reads back as the ending it always was.
        let mut future = bson::serialize_to_document(&ended).unwrap();
        future.insert("suspended_at", 3i64);
        assert_eq!(
            bson::deserialize_from_document::<ExecResp>(future).unwrap(),
            ended
        );
    }
}
