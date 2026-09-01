//! How something ended: what it produced, or why it produced nothing.
//!
//! An [`Outcome`] is `result` xor `error` — never both, never neither — and [`Error`] is
//! the second of those with a numeric [`code`](Error::code) a program can branch on.
//!
//! # Why this is not part of the envelope
//!
//! It was, and for as long as an outcome only ever came back it belonged there: `result`
//! and `error` are two of a *response's* members, and a response is one of
//! [`Message`](super::Message)'s three shapes.
//!
//! [`ExecCmd::Resume`](super::ExecCmd::Resume) is what changed that. A delegated call runs
//! in the client, and how it ended is the whole of what the `exec` carrying it back has to
//! say — so an outcome travels inside a **request** too, which is why it has a serde impl
//! of its own and a file of its own. It is no longer "what a response carries"; it is how
//! anything in this protocol ended, whichever direction it is travelling.
//!
//! And it has to be an outcome there rather than an [`ExecResult`](super::ExecResult),
//! because a delegated call can fail to produce one at all — see
//! [`ExecCmd::Resume`](super::ExecCmd::Resume).
//!
//! One rule about it is shared with the envelope rather than written twice — see
//! [`Outcome::from_members`].

use std::fmt;

use bson::Bson;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, DeserializeOwned, MapAccess, Visitor},
    ser::SerializeMap,
};

/// A `result` or an `error` — never both, never neither.
///
/// The `result` is held as a [`Bson`] because its type depends on the method the `id`
/// was issued for, which only the end that issued it knows. [`take`](Outcome::take) is
/// where that knowledge is applied.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Result(Bson),
    Error(Error),
}

/// Why a request could not be answered with a result.
///
/// `code` is what a program branches on and `message` is what a person reads. `data` is
/// anything extra the sender thought was worth carrying; nothing in this protocol
/// requires it.
///
/// `data` is boxed because a [`Bson`] is 112 bytes — it has to be wide enough for the
/// widest thing BSON can say, and this protocol says almost none of them — where the
/// whole of the rest of an `Error` is 32. Unboxed it made every `Result<_, Failure>` in
/// the crate carry 144 bytes to describe a failure that is nearly always a code and a
/// sentence. The box costs an allocation only when `data` is actually there, which so
/// far is never, and it is invisible on the wire: `Option<Box<T>>` and `Option<T>`
/// serialize the same.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Error {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Box<Bson>>,
}

impl Error {
    /// `exec`: the execution outlived its [`timeout_ms`](super::Exec::timeout_ms) and
    /// was killed.
    ///
    /// An error rather than a result, because there is no result: a killed command
    /// has no exit code, and whatever it had written is gone with it — one message
    /// cannot carry an ending that never happened. A requester acts on this
    /// specifically, which is what the code is for: retry with more time, or give
    /// up.
    pub const TIMED_OUT: i64 = -32000;

    /// `exec`: the program was not there, or could not be started.
    pub const NOT_EXECUTABLE: i64 = -32001;

    /// `exec`, `read`, `write`: the backend could not be brought up.
    ///
    /// Booting is nobody's own request —
    /// [`Start`](super::Notification::Start) only asks for it early, and anything that
    /// needs a booted session boots one — so this is reported to whoever asked for the
    /// call that needed it. Which is the point of it having a code: that requester is
    /// waiting on something, and this says the failure was the session's rather than
    /// the command's or the path's.
    pub const BOOT_FAILED: i64 = -32002;

    /// `read`, `write`: nothing is at the path — for a `write`, that means a
    /// directory above it, since the file itself is created if it is missing.
    pub const NOT_FOUND: i64 = -32005;

    /// `read`, `write`: the path is a directory, which has no bytes either way.
    ///
    /// Apart from [`NOT_FOUND`](Self::NOT_FOUND) because it says the opposite thing
    /// about the path: the name is taken, and by something a retry will not turn into
    /// a file.
    pub const IS_A_DIRECTORY: i64 = -32006;

    /// `read`, `write`: the path named a file the executor could not go on to read or
    /// write — permissions, a full disk, a backend that went away mid-operation.
    ///
    /// A `write` that fails this way says nothing about how much of `data` landed. The
    /// file is whatever it is, and a requester that needs to know asks with a `read`.
    pub const IO_FAILED: i64 = -32007;

    /// `init`: a workfs URL whose scheme this server has no provider for, named in the
    /// message.
    ///
    /// `init`'s own, and not deferred to the call that needs a session, because it is
    /// knowable the moment the frame is read: which kinds a server can realize is a fact
    /// about the *build*, and answering a path for a tree that can never be there would be
    /// a session in which every later path is a lie.
    ///
    /// Distinct from [`BOOT_FAILED`](Self::BOOT_FAILED) because the fix differs: the workfs
    /// is well formed and the server is the wrong build for it — a different binary, or a
    /// different URL.
    pub const UNSUPPORTED_WORKFS: i64 = -32008;

    /// `exec`, `read`, `write`: the workfs could not be put where `init` said it would be —
    /// no mount binding compiled in, no FUSE provider installed, the mount point busy, the
    /// store itself unreachable.
    ///
    /// Deferred like [`BOOT_FAILED`](Self::BOOT_FAILED) and for the same reason: mounting
    /// happens when the session boots, so this reaches whoever asked for the call that
    /// needed one. Apart from it because the kind of fix differs — the session is described
    /// correctly and the environment is what has to change.
    pub const MOUNT_FAILED: i64 = -32009;

    /// `init`: a network reach this server cannot provide, named in the message.
    ///
    /// `init`'s own, for the reason [`UNSUPPORTED_WORKFS`](Self::UNSUPPORTED_WORKFS) is:
    /// which reaches a server can answer is a fact about the build and the machine, knowable
    /// the moment the frame is read.
    ///
    /// Two things arrive as this. A name nobody has heard of — a client asking for something
    /// no backend implements. And a name that is understood and cannot be honoured: a server
    /// whose commands run on this host cannot take the network away from them, so it refuses
    /// every reach but `full` rather than pretending. Both are the same fix — ask for
    /// something else, or run against a different backend — which is why they are one code.
    ///
    /// Never used to *narrow* a session. A server that could give less than was asked for
    /// refuses instead: quietly granting a different reach than the one named is the failure
    /// [`Init::network`](crate::console::Init::network) exists to prevent.
    pub const UNSUPPORTED_NETWORK: i64 = -32010;

    /// The four the spec defines that a peer of ours can hit. `-32700` (parse
    /// error) belongs to whoever reads the frame, not here.
    /// The base image a session asked for is one this backend cannot give it.
    ///
    /// Not a reference that could not be fetched, which is a boot that failed: this is a
    /// backend with no base to swap at all, because its commands run on the server's own
    /// filesystem. Said at `init`, while the client can still ask for something else.
    pub const UNSUPPORTED_IMAGE: i64 = -32011;

    /// The session named a built image this server does not have.
    ///
    /// Distinct from [`UNSUPPORTED_IMAGE`](Self::UNSUPPORTED_IMAGE), which says the backend
    /// can swap no base at all: a client hearing this one can build the thing, where a client
    /// hearing that one has to ask for something else.
    ///
    /// Said at `init`, unlike a reference that cannot be fetched — which is a boot that
    /// failed. The difference is what finding out costs: whether an image built here is still
    /// here is a file test, where whether a registry has one is a network round trip that
    /// belongs to a boot.
    pub const UNKNOWN_IMAGE: i64 = -32012;

    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;

    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Error {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for Error {}

impl Outcome {
    /// The error, if this is an error response.
    pub fn error(&self) -> Option<&Error> {
        match self {
            Outcome::Error(error) => Some(error),
            Outcome::Result(_) => None,
        }
    }

    /// The `result`, as the type the method returns — `Progress` for `exec`, `InitResult`
    /// for `init`.
    ///
    /// The caller supplies `T` because the caller is the end that issued the `id`
    /// and so is the only one that knows the method. A `result` that will not
    /// deserialize is reported as an [`INTERNAL_ERROR`](Error::INTERNAL_ERROR),
    /// because from here it is indistinguishable from a peer that answered the
    /// wrong request — either way there is nothing usable and the reason belongs in
    /// a log.
    pub fn take<T: DeserializeOwned>(self) -> Result<T, Error> {
        match self {
            Outcome::Error(error) => Err(error),
            Outcome::Result(value) => bson::deserialize_from_bson(value).map_err(|e| {
                Error::new(
                    Error::INTERNAL_ERROR,
                    format!("result is not what this method returns: {e}"),
                )
            }),
        }
    }

    /// The one of the two members that is there, or `None` for neither.
    ///
    /// The xor is the whole of what this enforces, and it is enforced in one place
    /// because there are two that read these members: [`Message`](super::Message)'s
    /// deserializer, which finds them among a response's own members, and
    /// [`Outcome`]'s, which finds them nested in an `exec`'s
    /// [`cmd`](super::ExecCmd::Resume).
    ///
    /// Neither is `None` rather than an error because the two callers mean different
    /// things by it. A message with no `result` and no `error` has no `method` either,
    /// so what is wrong with it is that it is none of the three shapes; an outcome with
    /// neither is an outcome, and simply empty.
    pub(super) fn from_members<E: de::Error>(
        result: Option<Bson>,
        error: Option<Error>,
    ) -> Result<Option<Outcome>, E> {
        match (result, error) {
            (Some(value), None) => Ok(Some(Outcome::Result(value))),
            (None, Some(error)) => Ok(Some(Outcome::Error(error))),
            (Some(_), Some(_)) => Err(E::custom("both a result and an error")),
            (None, None) => Ok(None),
        }
    }
}

/// `{"result": ..}` or `{"error": ..}` — an object of exactly the member that is there.
///
/// Separate from [`Message`](super::Message)'s serializer, which writes the same two
/// members but *into* the response object rather than into one of their own. Sharing
/// would put a nested `{"result": {"result": ..}}` on the wire for a response, or need a
/// flattening helper for `params`; two small impls are the cheaper of the two. The rule
/// they share is [`from_members`](Outcome::from_members).
impl Serialize for Outcome {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(1))?;
        match self {
            Outcome::Result(value) => map.serialize_entry("result", value)?,
            Outcome::Error(error) => map.serialize_entry("error", error)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Outcome {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Outcome, D::Error> {
        d.deserialize_map(OutcomeVisitor)
    }
}

struct OutcomeVisitor;

impl<'de> Visitor<'de> for OutcomeVisitor {
    type Value = Outcome;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("an object with a result or an error")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Outcome, A::Error> {
        let mut result: Option<Bson> = None;
        let mut error: Option<Error> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "result" => result = Some(map.next_value()?),
                "error" => error = Some(map.next_value()?),
                // Ignored for the same reason the envelope ignores them.
                _ => {
                    map.next_value::<de::IgnoredAny>()?;
                }
            }
        }

        Outcome::from_members(result, error)?
            .ok_or_else(|| de::Error::custom("an outcome has no result and no error"))
    }
}

#[cfg(test)]
mod tests {
    use bson::{Document, doc};

    use super::{super::ExecResult, *};

    /// Every code is its own. Two that collided would be two failures a client could not
    /// tell apart, and the compiler has nothing to say about it.
    #[test]
    fn every_error_code_is_distinct() {
        use super::Error;
        let codes = [
            ("TIMED_OUT", Error::TIMED_OUT),
            ("NOT_EXECUTABLE", Error::NOT_EXECUTABLE),
            ("BOOT_FAILED", Error::BOOT_FAILED),
            ("NOT_FOUND", Error::NOT_FOUND),
            ("IS_A_DIRECTORY", Error::IS_A_DIRECTORY),
            ("IO_FAILED", Error::IO_FAILED),
            ("UNSUPPORTED_WORKFS", Error::UNSUPPORTED_WORKFS),
            ("MOUNT_FAILED", Error::MOUNT_FAILED),
            ("UNSUPPORTED_NETWORK", Error::UNSUPPORTED_NETWORK),
            ("UNSUPPORTED_IMAGE", Error::UNSUPPORTED_IMAGE),
            ("UNKNOWN_IMAGE", Error::UNKNOWN_IMAGE),
            ("INVALID_REQUEST", Error::INVALID_REQUEST),
            ("METHOD_NOT_FOUND", Error::METHOD_NOT_FOUND),
            ("INVALID_PARAMS", Error::INVALID_PARAMS),
            ("INTERNAL_ERROR", Error::INTERNAL_ERROR),
        ];
        let mut seen = std::collections::HashMap::new();
        for (name, code) in codes {
            if let Some(taken) = seen.insert(code, name) {
                panic!("{code} is both {taken} and {name}");
            }
        }
    }

    /// Read an outcome off the bytes a document makes, which is what a peer would
    /// actually have sent — the whole point of going through the wire rather than
    /// straight from a [`Bson`] is that this is the path the reader takes.
    fn read(doc: Document) -> Result<Outcome, bson::error::Error> {
        bson::deserialize_from_slice(&bson::serialize_to_vec(&doc).unwrap())
    }

    /// A response's `result` is typed by the method its id was issued for, which only
    /// the end that issued it knows.
    #[test]
    fn a_result_is_typed_by_the_method_the_caller_remembers() {
        let result = Outcome::Result(
            bson::serialize_to_bson(&ExecResult {
                code: 3,
                stdout: b"out".to_vec(),
                ..ExecResult::default()
            })
            .unwrap(),
        );
        assert_eq!(result.clone().take::<ExecResult>().unwrap().code, 3);

        // Asking for the wrong type is a peer that answered the wrong request as
        // far as anyone here can tell.
        let wrong = result.take::<()>().unwrap_err();
        assert_eq!(wrong.code, Error::INTERNAL_ERROR);

        let error = Outcome::Error(Error::new(Error::TIMED_OUT, "killed"));
        assert_eq!(error.error().unwrap().code, Error::TIMED_OUT);
        assert_eq!(
            error.take::<ExecResult>().unwrap_err().code,
            Error::TIMED_OUT
        );
    }

    /// An outcome on its own, which is how an `exec`'s
    /// [`cmd`](super::super::ExecCmd::Resume) carries one: the member that is there and no
    /// other, and the xor enforced both ways.
    #[test]
    fn an_outcome_is_the_one_member_that_is_there() {
        let wire = |outcome: &Outcome| bson::serialize_to_document(outcome).unwrap();

        assert_eq!(
            wire(&Outcome::Result(doc! {"code": 0i64}.into())),
            doc! {"result": {"code": 0i64}},
        );
        assert_eq!(
            wire(&Outcome::Error(Error::new(Error::TIMED_OUT, "too slow"))),
            doc! {"error": {"code": -32000i64, "message": "too slow"}},
        );

        for outcome in [
            Outcome::Result(Bson::Null),
            Outcome::Error(Error::new(Error::BOOT_FAILED, "no kvm")),
        ] {
            assert_eq!(read(wire(&outcome)).unwrap(), outcome);
        }

        let refused = |doc: Document, because: &str| {
            let error = read(doc.clone())
                .expect_err(&format!("accepted {doc:?}"))
                .to_string();
            assert!(error.contains(because), "{doc:?} → {error}");
        };

        refused(Document::new(), "no result and no error");
        refused(
            doc! {"result": Bson::Null, "error": {"code": 1i64, "message": "x"}},
            "both a result and an error",
        );
        // Unknown members are ignored here too, so a peer may add one.
        assert_eq!(
            read(doc! {"result": Bson::Null, "trace_id": "abc"}).unwrap(),
            Outcome::Result(Bson::Null),
        );
    }
}
