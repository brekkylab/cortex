//! The methods something answers, and everything they carry.
//!
//! A [`Call`] is a `method` and its `params`, and the payloads here are the two
//! halves of one: what a request sends ([`Start`], [`Exec`]) and what its response
//! brings back ([`ExecResult`]). They live together because a `result` is typed by
//! the method its `id` was issued for, so the pair is the thing worth reading at
//! once.
//!
//! [`Notification`](super::Notification) is the other half of the protocol: the
//! methods nothing answers.

use serde::de::{self, DeserializeOwned};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Method;

/// A method and its parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// **client → server.** Boot, and make these names runnable.
    Start(Start),

    /// **either direction.** Run this.
    Exec(Exec),

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

/// One thing to run, and everything needed to run it. The `params` of `exec`.
///
/// On a console channel it is the command the session exists for. On a delegate
/// channel it is a delegated executable, and `cmd[0]` is one of the names that
/// arrived in [`Start`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exec {
    /// Already split into argv. Nothing here consults a shell, so quoting and word
    /// rules stay wherever the command was composed; a caller that wants shell
    /// semantics asks for them outright — `["sh", "-c", "..."]`.
    pub cmd: Vec<String>,

    // There is no working directory, so an execution runs wherever the executor
    // runs things and a relative path in `cmd` is resolved there. Sending one
    // would be a host path, which means nothing inside a micro-VM guest, so
    // saying where to run something only becomes expressible once both ends
    // agree on what a path is — which is what a workspace volume mount is for,
    // and where it belongs.
    /// Everything the command will read on its standard input, all of it, now.
    ///
    /// Here rather than in messages of its own because output is not streamed
    /// either: a command whose input depends on its output cannot be driven across
    /// this wire whichever way the input arrives, so the simpler shape is the
    /// honest one.
    ///
    /// Empty means immediate EOF, which is also what a command that reads nothing
    /// wants. There is no distinction between no input and no bytes of input.
    #[serde(default, with = "bytes", skip_serializing_if = "Vec::is_empty")]
    pub stdin: Vec<u8>,

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

pub enum ExecResultV2 {
    Done(ExecResult),
    Delegated(Exec),
}

/// A whole execution in one value: everything it wrote, and how it ended. The
/// `result` of `exec`.
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
            Call::Stop => Ok(()),
        }
    }

    /// The call a `method` and its `params` name.
    ///
    /// Reached only for a message that carried an `id`, which is what says it is a
    /// request at all — so a notification's method arriving here is a peer asking
    /// to be answered about something nothing answers.
    pub(super) fn from_params<E: de::Error>(method: Method, params: Value) -> Result<Self, E> {
        Ok(match method {
            Method::Start => Call::Start(typed_params(method, params)?),
            Method::Exec => Call::Exec(typed_params(method, params)?),
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
fn typed_params<T: DeserializeOwned, E: de::Error>(method: Method, params: Value) -> Result<T, E> {
    serde_json::from_value(params).map_err(|e| E::custom(format!("{method} params: {e}")))
}

/// Raw bytes, encoded the way the codec in use wants them.
///
/// JSON has no byte type, so `serialize_bytes` there would become an array of
/// numbers: `[104,105,10]`, four characters per byte, for the bulk of what this
/// channel carries. So base64 instead — 1.37× rather than 4×, and it survives a
/// `Vec<u8>` that is not UTF-8, which output routinely is not.
///
/// The codec is asked rather than assumed
/// ([`is_human_readable`](serde::Serializer::is_human_readable)) because JSON-RPC
/// does not require JSON: CBOR and MessagePack carry the same objects and have a
/// native byte type, and on those the bytes go across as themselves.
mod bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::de::{SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&STANDARD.encode(bytes))
        } else {
            s.serialize_bytes(bytes)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        if d.is_human_readable() {
            d.deserialize_str(Base64)
        } else {
            d.deserialize_byte_buf(Raw)
        }
    }

    struct Base64;

    impl<'de> Visitor<'de> for Base64 {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("base64")
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Vec<u8>, E> {
            STANDARD.decode(v).map_err(E::custom)
        }
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

        /// A codec that writes bytes as a sequence reads them back as one.
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

    /// Not utf-8, contains NUL and a newline: nothing about program bytes is
    /// special, and JSON carries them as base64 rather than as an array of
    /// numbers.
    #[test]
    fn program_bytes_survive_as_base64() {
        let bytes = vec![0xff, 0xfe, 0x00, b'\n', 0x00];
        let value = serde_json::to_value(ExecResult {
            stdout: bytes.clone(),
            stderr: bytes.clone(),
            ..ExecResult::default()
        })
        .unwrap();
        assert_eq!(value["stdout"], "//4ACgA=");

        let read: ExecResult = serde_json::from_value(value).unwrap();
        assert_eq!((read.stdout, read.stderr), (bytes.clone(), bytes));
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
