//! The methods something answers, and everything they carry.
//!
//! A [`Call`] is a `method` and its `params`, and the payloads here are the two
//! halves of one: what a request sends ([`Start`], [`Exec`]) and what its response
//! brings back ([`Progress`], and the [`ExecResult`] inside it). They live together
//! because a `result` is typed by the method its `id` was issued for, so the pair is
//! the thing worth reading at once.
//!
//! [`Notification`](super::Notification) is the other half of the protocol: the
//! methods nothing answers.

use bson::Bson;
use serde::de::{self, DeserializeOwned};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};

use super::{Method, Outcome};

/// A method and its parameters.
///
/// All of them are the client's: the client asks and the server answers, and that is
/// true of every method here. Nothing the server has to say arrives as a request of
/// its own — see [`Progress`] for how a server that needs something from the client
/// asks for it while remaining the answering end.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    /// **client → server.** Boot, and make these names runnable.
    Start(Start),

    /// **client → server.** Run this.
    Exec(Exec),

    /// **client → server.** The delegated call you asked for ended like this; carry
    /// on.
    ///
    /// The other half of [`Progress::Delegated`], and only ever a reply to one: an
    /// execution that paused for a delegated call is waiting for exactly this, and
    /// there is never more than one paused at a time. So nothing identifies *which*
    /// delegated call this answers — the id of the response that asked for it does,
    /// and there is only one outstanding.
    ///
    /// An [`Outcome`] and not an [`ExecResult`] because a delegated call can fail to
    /// produce one at all: a name the client does not have is
    /// [`NOT_EXECUTABLE`](super::Error::NOT_EXECUTABLE) and one that ran too long is
    /// [`TIMED_OUT`](super::Error::TIMED_OUT), and the server hands whichever it gets
    /// straight to the shim that is waiting.
    Resume(Outcome),

    /// **client → server.** Release what [`Start`] booted.
    ///
    /// The other half of booting, and much what stopping a VM is: the guest goes
    /// away, the socket and the symlinks go with it, and a scratch directory is
    /// cleaned up by whoever made it rather than left for someone to find later.
    /// Backends differ in what that costs, which is exactly why the client asks
    /// rather than assuming.
    ///
    /// Afterwards the session is back where it was before [`Start`], so another
    /// one is allowed: a server process can outlive the resources it booted, which
    /// is worth something when booting is the expensive part. Ending the *process*
    /// is [`Quit`](super::Notification::Quit).
    ///
    /// Nothing here is about a single execution. To give up on one of those, let
    /// its [`timeout_ms`](Exec::timeout_ms) expire.
    Stop,
}

/// Boot, and make these names runnable. The `params` of `start`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    /// Sorted and without duplicates. Empty is not an error — a client with
    /// nothing to delegate is still a client.
    ///
    /// What running one *means* is not here and cannot be: the behaviour lives in
    /// the client's [`ExecutableSet`](crate::executable::ExecutableSet), so a
    /// server only arranges for something that runs the name to reach the client —
    /// on a channel of its own, as an `exec` like any other.
    pub delegated: Vec<String>,

    /// The default for [`Exec::timeout_ms`], for executions that do not set one.
    ///
    /// `None` is no default at all, and then an execution without its own timeout
    /// runs until it finishes — or forever, if it is the kind of command that does
    /// not finish. Sending it here rather than on every [`Exec`] is the point: a
    /// client that never wants to wait forever says so once, including for the
    /// delegated calls it did not write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_timeout_ms: Option<u64>,
}

/// One thing to run, and everything needed to run it. The `params` of `exec`, and
/// what a [`Progress::Delegated`] carries back the other way.
///
/// As `params` it is the command the session exists for. Inside a `Delegated` it is a
/// delegated executable the server cannot run itself, and `cmd[0]` is one of the
/// names that arrived in [`Start`].
///
/// One type for both because an execution request is an execution request no matter
/// who is asking whom: a command, output, a code at the end.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exec {
    /// Already split into argv. Nothing here consults a shell, so quoting and word
    /// rules stay wherever the command was composed; a caller that wants shell
    /// semantics asks for them outright — `["sh", "-c", "..."]`.
    pub cmd: Vec<String>,

    /// How long this may run before the executor kills it, in milliseconds. `None`
    /// falls back to [`Start::default_timeout_ms`], and if that is `None` too
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

/// An argv and the defaults for the rest, over the three ways a caller has an argv: a
/// literal, a slice, a `Vec`.
///
/// Which is what most executions are — `["echo", "hi"]` rather than an `Exec` written out
/// to say only that. Anything else to set is the struct, as before.
fn argv<S: AsRef<str>>(cmd: impl IntoIterator<Item = S>) -> Exec {
    Exec {
        cmd: cmd.into_iter().map(|s| s.as_ref().to_string()).collect(),
        ..Exec::default()
    }
}

impl<S: AsRef<str>, const N: usize> From<[S; N]> for Exec {
    fn from(cmd: [S; N]) -> Exec {
        argv(cmd)
    }
}

impl<S: AsRef<str>> From<&[S]> for Exec {
    fn from(cmd: &[S]) -> Exec {
        argv(cmd)
    }
}

impl<S: AsRef<str>> From<Vec<S>> for Exec {
    fn from(cmd: Vec<S>) -> Exec {
        argv(cmd)
    }
}

/// How far an execution got: finished, or waiting for the client. The `result` of
/// both `exec` and `resume`.
///
/// # Why a result and not a request
///
/// A delegated executable's behaviour lives in the *client* — a Rust closure, an HTTP
/// call, whatever the host wants a tool to mean — so a command that invokes one by
/// name cannot be finished by the server alone. The server has to ask.
///
/// It asks by answering. [`Delegated`](Self::Delegated) is a complete, ordinary
/// response to the request the client is already waiting on, and it means *this
/// execution is not over and here is what I need from you*. The client runs the name,
/// says so with [`Resume`](Call::Resume), and gets the next `Progress` back. The chain
/// ends at [`Done`](Self::Done).
///
/// So the server never issues a request and the client never answers one. Every
/// channel in the system has one end that only asks and one that only answers, which
/// is what there is to gain: no end needs a pending table, no end has a thread waiting
/// on something only that thread could read, and there is one channel rather than one
/// per delegated call.
///
/// # What it costs
///
/// **Delegated calls are served one at a time.** A response can carry one of these,
/// so a command that starts several delegated executables together (`foo & bar`,
/// `make -j8`) has them run in turn rather than at once. The [`timeout_ms`](Exec::timeout_ms)
/// of the outer execution has to cover the sum.
///
/// That is latency and not a deadlock, and only because delegated calls are
/// independent of each other: nothing an [`Exec`] carries is input, so no delegated
/// call is waiting on another being served first. **If input ever reaches a delegated
/// call, serving them in turn stops being safe** and this is the line of reasoning
/// that has to change.
///
/// A client with no delegated names never sees anything but `Done`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Progress {
    /// The execution is over, and this is all of it.
    Done(ExecResult),

    /// The execution is paused on a delegated executable. Run it and
    /// [`Resume`](Call::Resume).
    ///
    /// Nothing else may be asked until then: the execution owes an answer that has not
    /// been sent, so a client that asks about anything else is asking about a request
    /// it has not been answered on.
    Delegated(Exec),
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

impl Call {
    pub fn method(&self) -> Method {
        match self {
            Call::Start(_) => Method::Start,
            Call::Exec(_) => Method::Exec,
            Call::Resume(_) => Method::Resume,
            Call::Stop => Method::Stop,
        }
    }

    /// Writes this call's `params` into the object being serialized.
    ///
    /// Here rather than in [`Message`](super::Message)'s serializer because what a
    /// method's parameters are is this type's business, and a variant added later
    /// should have one place to say so. `params` is omitted rather than null for a
    /// method that takes none: the spec says it may be left out, and `null` is not
    /// one of the two types it allows.
    pub(super) fn serialize_params<M: SerializeMap>(&self, map: &mut M) -> Result<(), M::Error> {
        match self {
            Call::Start(start) => map.serialize_entry("params", start),
            Call::Exec(exec) => map.serialize_entry("params", exec),
            Call::Resume(outcome) => map.serialize_entry("params", outcome),
            Call::Stop => Ok(()),
        }
    }

    /// The call a `method` and its `params` name.
    ///
    /// Reached only for a message that carried an `id`, which is what says it is a
    /// request at all — so a notification's method arriving here is a peer asking
    /// to be answered about something nothing answers.
    pub(super) fn from_params<E: de::Error>(method: Method, params: Bson) -> Result<Self, E> {
        Ok(match method {
            Method::Start => Call::Start(typed_params(method, params)?),
            Method::Exec => Call::Exec(typed_params(method, params)?),
            Method::Resume => Call::Resume(typed_params(method, params)?),
            Method::Stop => Call::Stop,
            Method::Quit => {
                return Err(E::custom("quit is a notification and cannot carry an id"));
            }
        })
    }
}

impl Exec {
    /// The program and its arguments, or `None` for an empty `cmd`.
    ///
    /// The split every executor needs, done once here rather than at each of them
    /// — and an empty command is refused rather than handed on as a program nobody
    /// can run.
    pub fn split(&self) -> Option<(&String, &[String])> {
        self.cmd.split_first()
    }
}

/// A method's `params`, as the type that method takes.
///
/// Generic because the two methods that take parameters take different ones, and
/// the error names the method so a rejection says which shape was expected.
fn typed_params<T: DeserializeOwned, E: de::Error>(method: Method, params: Bson) -> Result<T, E> {
    bson::deserialize_from_bson(params).map_err(|e| E::custom(format!("{method} params: {e}")))
}

/// Raw bytes, as themselves.
///
/// BSON has a byte type — `Binary`, subtype `Generic` — so the bulk of what this
/// channel carries goes across at 1.0× rather than as text. This is the whole reason
/// the codec is BSON and not JSON: JSON has no byte type, so `stdout` had to be
/// base64 (1.37×) to avoid being an array of numbers (`[104,105,10]`, 4×).
///
/// # Why this does not ask the codec
///
/// It used to. [`is_human_readable`](serde::Serializer::is_human_readable) was how a
/// textual codec got base64 and a binary one got bytes, from one helper.
///
/// That branch cannot stay, because it silently loses. A message's `params` and
/// `result` are held as a [`Bson`] before they reach the wire — they have to be, since
/// a `result` is typed by a method only the caller knows — and `bson`'s *value-level*
/// serializer reports `is_human_readable() == true`. So the branch would encode base64
/// on the way into the `Bson`, and the wire would faithfully carry a string: 1.37×, the
/// byte type unused, and nothing to show it had happened. `bson`'s `SerializerOptions`
/// is `pub(crate)`, so it cannot be told otherwise.
///
/// One codec, and it has bytes. If a textual wire is ever wanted for a person to read,
/// BSON's own projection is the thing to reach for — `Bson::into_relaxed_extjson`
/// spells `Binary` as `{"$binary": ..}` — rather than a second spelling in here.
mod bytes {
    use serde::de::{SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        d.deserialize_byte_buf(Raw)
    }

    struct Raw;

    impl<'de> Visitor<'de> for Raw {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("bytes")
        }

        fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
            Ok(v.to_vec())
        }

        fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
            Ok(v)
        }

        /// A peer that spelled its bytes as an array is read rather than refused: BSON
        /// has arrays too, and `[104,105,10]` is unambiguous even though nothing here
        /// writes it.
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(b) = seq.next_element()? {
                out.push(b);
            }
            Ok(out)
        }
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
    /// test exists to catch; see the `bytes` module on why the codec is not asked.
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

    /// An unset timeout is absent rather than null, which is what
    /// `skip_serializing_if` buys — and it reads back unset.
    #[test]
    fn no_timeout_is_no_member() {
        let value = bson::serialize_to_bson(&Exec::from(["ls"])).unwrap();
        let doc = value.as_document().unwrap();
        assert_eq!(doc.get("timeout_ms"), None);

        let read: Exec = bson::deserialize_from_bson(value).unwrap();
        assert_eq!(read, Exec::from(["ls"]));
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

    /// The three argv shapes are the one `Exec` written out, and nothing else is set.
    #[test]
    fn an_argv_is_an_exec_with_the_defaults() {
        let written = Exec {
            cmd: vec!["echo".into(), "hi".into()],
            ..Exec::default()
        };

        assert_eq!(Exec::from(["echo", "hi"]), written);
        assert_eq!(Exec::from(&["echo", "hi"][..]), written);
        assert_eq!(Exec::from(vec![String::from("echo"), "hi".into()]), written);
    }
}
