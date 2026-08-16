use std::fmt;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::bytes;
use crate::console::{Outcome, RequestId};

/// One step of an execution, and everything that step needs. The `params` of `exec`, and
/// what a [`Progress::Delegated`] carries back the other way.
///
/// As `params` it is either the command the session exists for or the answer a paused
/// execution is waiting on, and [`cmd`](Self::cmd) is which. Inside a `Delegated` it is a
/// delegated executable the server cannot run itself, and the first word of its command is
/// one of the names that arrived in [`Init`](super::Init).
///
/// One type for all of them because an execution request is an execution request no matter
/// who is asking whom: a command, output, a code at the end.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Exec {
    /// What this `exec` asks for, and the only thing that says which of the two it is.
    pub cmd: ExecCmd,

    /// How long this may run before the executor kills it, in milliseconds. `None`
    /// falls back to the default the session was announced with in [`Init`](super::Init),
    /// and if there is none there is no limit.
    ///
    /// It bounds a [`New`](ExecCmd::New) and nothing else. A [`Resume`](ExecCmd::Resume)
    /// is not an execution of its own but the next step of one already running under the
    /// timeout of the `exec` that started it, and a second bound on the same execution is
    /// two answers to one question.
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

    /// Where the command was invoked — a path **in the server's filesystem**, under the
    /// [`path`](super::VolumeMount::path) `init` answered with.
    ///
    /// It is what makes a delegated call's relative paths resolvable. The client runs the
    /// executable in a process of its own, so an argv alone would name a file relative to
    /// nothing.
    ///
    /// The server's own path and not a name in some namespace both ends agree on, for the
    /// reason every other path here is: there is one tree, the server said where it is, and
    /// a spelling that had to be rewritten on the way past would be a second thing for the
    /// two ends to disagree about.
    ///
    /// **Reported, never instructed.** A shim fills it because it knows where it stood, and
    /// a client reads it off a [`Delegated`](Progress::Delegated). A client's own
    /// [`New`](ExecCmd::New) leaves it `None`, because a caller-chosen directory would be a
    /// second way to decide where a command runs beside the tree that already decides it.
    ///
    /// `None` when nothing reported one, or when the directory's name has no `String` form
    /// — a path inside a passthrough store may legitimately not be UTF-8. It is never
    /// *guessed*: substituting the root would name a different file and say nothing about
    /// having done so, which is the failure this field exists to prevent.
    ///
    /// Like [`timeout_ms`](Self::timeout_ms) it belongs to a `New` and means nothing on a
    /// [`Resume`](ExecCmd::Resume) — and, like it, nothing enforces that. Putting it inside
    /// `New` would make that variant an object, and the shape of `cmd`'s value is the only
    /// thing telling the two apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// A command to run, or the answer a paused execution is waiting for.
///
/// The two things an `exec` can be asking for, and the whole difference between them. A
/// [`New`](Self::New) starts an execution; a [`Resume`](Self::Resume) carries one on past
/// the delegated call it stopped at, which is what makes a paused execution resumable
/// without a method of its own — see [`Progress`] for why the server asks by answering.
///
/// # Why the shape says which
///
/// Neither variant is tagged, because neither needs to be: an argv is an array and an
/// answer is an object, and a reader with the value in front of it has already been told
/// which one it has. So `cmd` alone decides it, and there is nothing to keep agreeing with
/// — no discriminant a peer could spell one way while the value beside it says the other.
///
/// The spelling this replaces is a `cmd` with an optional answer beside it, and what that
/// costs is states nobody means. Both members present is a command that is also an answer,
/// neither is an execution asking for nothing, and every reader of an `exec` has to decide
/// what to do about each — where two shapes have exactly the two cases there are. An empty
/// argv is still refusable on its own terms, and is refused where a command is turned into
/// a process rather than everywhere an `exec` is read.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecCmd {
    /// Run this. Already split into argv, and nothing here consults a shell, so quoting
    /// and word rules stay wherever the command was composed; a caller that wants shell
    /// semantics asks for them outright — `["sh", "-c", "..."]`.
    New(Vec<String>),

    /// The delegated call this execution carries on from: which one it was, and how it
    /// ended.
    ///
    /// A server that answered [`Progress::Delegated`] is holding an execution that owes
    /// output it cannot produce, and the `exec` carrying this is the client handing that
    /// output back. It asks for nothing new to be run — what it adds is the answer the
    /// execution on the far end is already waiting for.
    Resume {
        /// The id of the request whose response asked for the delegated call, which is what
        /// says *which* one this answers.
        ///
        /// There is never more than one paused at a time, so it identifies nothing the
        /// server could not have worked out — what it earns is the same thing an `id` earns
        /// anywhere here: an answer carrying a number nobody is waiting on is a peer that
        /// has lost its place, and can be refused rather than mistaken for the one that was
        /// due.
        id: RequestId,

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
        outcome: Outcome,
    },
}

impl Default for ExecCmd {
    /// A command with no argv, which is what an [`Exec`] with nothing filled in asks for.
    fn default() -> ExecCmd {
        ExecCmd::New(Vec::new())
    }
}

impl ExecCmd {
    /// The program and its arguments, for a step that runs something.
    ///
    /// The split every executor needs, done once here rather than at each of them — and
    /// an empty command is refused rather than handed on as a program nobody can run.
    ///
    /// `None` for a [`Resume`](Self::Resume) as well, since it runs nothing either. An end
    /// that has to tell those apart matches on the variant instead: a resume runs nothing
    /// on purpose, where an empty argv is a command that was never usable.
    pub fn split(&self) -> Option<(&String, &[String])> {
        match self {
            ExecCmd::New(argv) => argv.split_first(),
            ExecCmd::Resume { .. } => None,
        }
    }
}

/// An array for a command, an object for an answer — see [`ExecCmd`] for why that is all
/// the tagging there is.
///
/// Written out rather than derived as `untagged`, because serde's untagged representation
/// buffers the whole value into an intermediate and then tries each variant against it.
/// What this one carries is an [`Outcome`], whose `result` is a [`Bson`](bson::Bson) of
/// whatever a method returned — the bytes of a delegated call's output among them. Reading
/// the wire once is both the cheaper path and the one whose failure says where it was.
impl Serialize for ExecCmd {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            ExecCmd::New(argv) => argv.serialize(s),
            ExecCmd::Resume { id, outcome } => {
                let mut map = s.serialize_map(Some(2))?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("outcome", outcome)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ExecCmd {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<ExecCmd, D::Error> {
        d.deserialize_any(ExecCmdVisitor)
    }
}

struct ExecCmdVisitor;

impl<'de> Visitor<'de> for ExecCmdVisitor {
    type Value = ExecCmd;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an argv array, or an object saying how a delegated call ended")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ExecCmd, A::Error> {
        let mut argv = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(arg) = seq.next_element()? {
            argv.push(arg);
        }
        Ok(ExecCmd::New(argv))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ExecCmd, A::Error> {
        let mut id: Option<RequestId> = None;
        let mut outcome: Option<Outcome> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "id" => id = Some(map.next_value()?),
                "outcome" => outcome = Some(map.next_value()?),
                // Ignored for the same reason the envelope ignores them.
                _ => {
                    map.next_value::<de::IgnoredAny>()?;
                }
            }
        }

        // Both, or this is neither of the two things an `exec` can be: an outcome naming
        // no request is one nobody can match to a paused execution, and a request named
        // with nothing beside it says nothing about how the call went.
        Ok(ExecCmd::Resume {
            id: id.ok_or_else(|| de::Error::missing_field("id"))?,
            outcome: outcome.ok_or_else(|| de::Error::missing_field("outcome"))?,
        })
    }
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
/// sends another `exec` carrying the answer as an [`ExecCmd::Resume`], whose response is
/// the next `Progress`. The chain ends at [`Done`](Self::Done).
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
    /// carrying the answer as an [`ExecCmd::Resume`].
    ///
    /// Nothing else may be asked in between: the execution owes an answer that has not
    /// been sent, so a client that asks about anything else is asking about a request it
    /// has not been answered on.
    Delegated(Exec),
}

#[cfg(test)]
mod tests {
    use bson::{Bson, Document, doc};

    use super::*;
    use crate::console::Error;

    /// Read an `exec` off the bytes a document makes, which is what a peer would actually
    /// have sent.
    fn read(doc: Document) -> Result<Exec, bson::error::Error> {
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
        let value = bson::serialize_to_bson(&ExecResult {
            stdout: bytes.clone(),
            stderr: bytes.clone(),
            ..ExecResult::default()
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

        let read: ExecResult = bson::deserialize_from_bson(value).unwrap();
        assert_eq!((read.stdout, read.stderr), (bytes.clone(), bytes));
    }

    /// An execution that sets no timeout is a `cmd` and nothing else: the member is absent
    /// rather than null, which is what `skip_serializing_if` buys — and it reads back
    /// unset.
    #[test]
    fn an_unadorned_exec_is_a_command_and_nothing_else() {
        let exec = Exec {
            cmd: ExecCmd::New(vec!["ls".into()]),
            ..Exec::default()
        };
        let doc = bson::serialize_to_document(&exec).unwrap();
        assert_eq!(doc, doc! {"cmd": ["ls"]});
        assert_eq!(read(doc).unwrap(), exec);
    }

    /// Which of the two an `exec` is, is the shape of its `cmd` and nothing else: an array
    /// is a command, an object is how a delegated call ended, and each reads back as the
    /// one it was written as.
    #[test]
    fn what_an_exec_asks_for_is_the_shape_of_its_cmd() {
        let wire = |cmd: ExecCmd| {
            bson::serialize_to_document(&Exec {
                cmd,
                ..Exec::default()
            })
            .unwrap()
        };

        for cmd in [
            ExecCmd::New(vec!["ls".into()]),
            ExecCmd::New(Vec::new()),
            ExecCmd::Resume {
                id: 2,
                outcome: Outcome::Result(doc! {"code": 0i64}.into()),
            },
            ExecCmd::Resume {
                id: 2,
                outcome: Outcome::Error(Error::new(Error::NOT_EXECUTABLE, "no such name")),
            },
        ] {
            let doc = wire(cmd.clone());
            assert_eq!(read(doc.clone()).unwrap().cmd, cmd, "{doc:?}");
        }

        assert_eq!(wire(ExecCmd::New(vec!["ls".into()])), doc! {"cmd": ["ls"]});
        assert_eq!(
            wire(ExecCmd::Resume {
                id: 2,
                outcome: Outcome::Result(doc! {"code": 0i64}.into()),
            }),
            doc! {"cmd": {"id": 2i64, "outcome": {"result": {"code": 0i64}}}},
        );
    }

    /// Half an answer is not one: an outcome nobody can match to a paused execution, or an
    /// id with nothing to report.
    #[test]
    fn a_resume_needs_both_of_its_members() {
        let refused = |cmd: Document, because: &str| {
            let doc = doc! {"cmd": cmd};
            let error = read(doc.clone())
                .expect_err(&format!("accepted {doc:?}"))
                .to_string();
            assert!(error.contains(because), "{doc:?} → {error}");
        };

        refused(doc! {"outcome": {"result": Bson::Null}}, "id");
        refused(doc! {"id": 2i64}, "outcome");
        // The xor inside the outcome is the outcome's own rule, and applies here as it
        // does to a response's own two members.
        refused(doc! {"id": 2i64, "outcome": {}}, "no result and no error");
        // An unknown member is ignored here too, so a peer may add one.
        assert_eq!(
            read(doc! {"cmd": {"id": 2i64, "outcome": {"result": Bson::Null}, "took_ms": 3i64}})
                .unwrap()
                .cmd,
            ExecCmd::Resume {
                id: 2,
                outcome: Outcome::Result(Bson::Null),
            },
        );
    }

    /// An empty command is refused where a command becomes a process, and a resume runs
    /// nothing at all — the same `None` for two different reasons, which is why an end
    /// that means one of them matches on the variant.
    #[test]
    fn a_command_splits_into_a_program_and_its_arguments() {
        let cmd = ExecCmd::New(vec!["sh".into(), "-c".into(), "".into()]);
        let (program, args) = cmd.split().unwrap();
        assert_eq!(program, "sh");
        assert_eq!(args, ["-c", ""]);

        assert!(ExecCmd::default().split().is_none());
        assert!(
            ExecCmd::Resume {
                id: 0,
                outcome: Outcome::Result(Bson::Null),
            }
            .split()
            .is_none()
        );
    }

    /// The invariant a new sibling field must not disturb: `cmd`'s two variants are told
    /// apart by the shape of their *value* and nothing else, so a member beside `cmd`
    /// leaves that alone.
    #[test]
    fn a_new_field_does_not_disturb_the_shape_that_names_the_variant() {
        let exec = Exec {
            cmd: ExecCmd::New(vec!["ls".into()]),
            timeout_ms: Some(5_000),
            cwd: Some("/mnt/workfs/work/sub".into()),
        };
        let doc = bson::serialize_to_document(&exec).expect("serializes");
        assert!(
            doc.get_array("cmd").is_ok(),
            "a New's cmd is still an array"
        );

        let back = read(doc).expect("deserializes");
        assert!(
            matches!(back.cmd, ExecCmd::New(_)),
            "still reads back as New"
        );
        assert_eq!(back.cwd.as_deref(), Some("/mnt/workfs/work/sub"));
    }

    #[test]
    fn no_cwd_is_absent_from_the_frame() {
        let exec = Exec {
            cmd: ExecCmd::New(vec!["ls".into()]),
            ..Exec::default()
        };
        assert_eq!(
            bson::serialize_to_document(&exec).unwrap(),
            doc! {"cmd": ["ls"]}
        );
    }

    #[test]
    fn an_absent_cwd_reads_as_none() {
        let exec = read(doc! {"cmd": ["ls"]}).expect("deserializes");
        assert_eq!(exec.cwd, None);
    }
}
