//! Why a request could not be answered: a numeric code a program can branch on, and a
//! sentence a person reads.
//!
//! One end of a response's `result` xor `error`, and the only one that is a type here. The
//! other is whatever the method returns, and it stays a [`Bson`] until the end that issued
//! the `id` says which method that was — see [`Message`](super::Message).
//!
//! So the pair is spelled `Result<Bson, Error>` and not an enum of our own. A response
//! either carries what was asked for or says why it does not, which is what `Result` means
//! everywhere else in Rust; naming that shape again would be a second vocabulary for it,
//! and `?` would stop working on the way.
//!
//! The codes below are the whole of what a requester branches on. Adding a *shape* to say
//! what a number already says is the thing this file exists not to do.

use std::fmt;

use bson::Bson;
use serde::{Deserialize, Serialize};

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
    /// `exec`: the execution outlived its [`timeout_ms`](super::ExecCall::timeout_ms) and
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
    /// [`InitCall::network`](crate::console::InitCall::network) exists to prevent.
    pub const UNSUPPORTED_NETWORK: i64 = -32010;

    /// The four the spec defines that a peer of ours can hit. `-32700` (parse
    /// error) belongs to whoever reads the frame, not here.
    /// The base image a session asked for is one this backend cannot give it.
    ///
    /// Not a reference that could not be fetched, which is a boot that failed: this is a
    /// backend with no base to swap at all, because its commands run on the server's own
    /// filesystem. Said at `init`, while the client can still ask for something else.
    pub const UNSUPPORTED_IMAGE: i64 = -32011;

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
