//! Why a request could not be answered: a numeric code a program branches on, and a
//! sentence a person reads.
//!
//! The codes are the whole of what a requester branches on; a failure kind is a new code,
//! never a new message shape.

use std::fmt;

use bson::Bson;
use serde::{Deserialize, Serialize};

/// Why a request could not be answered with a result.
///
/// `data` is optional extra context; nothing in this protocol requires it.
///
/// `data` is boxed because a [`Bson`] is 112 bytes against 32 for the rest, which would
/// bloat every `Result<_, Failure>` to 144 bytes. The box allocates only when `data` is
/// present and is invisible on the wire.
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
    /// An error, not a result: a killed command has no exit code, and its output is
    /// discarded. Retry with more time, or give up.
    pub const TIMED_OUT: i64 = -32000;

    /// `exec`: the program was not there, or could not be started.
    pub const NOT_EXECUTABLE: i64 = -32001;

    /// `exec`, `read`, `write`: the backend could not be brought up.
    ///
    /// Booting happens on demand ([`Start`](super::Notification::Start) only asks early),
    /// so this goes to the call that needed the boot, and says the failure was the
    /// session's rather than the command's or the path's.
    pub const BOOT_FAILED: i64 = -32002;

    /// `read`, `write`: nothing is at the path. For a `write`, a parent directory is
    /// missing, since the file itself is created.
    pub const NOT_FOUND: i64 = -32005;

    /// `read`, `write`: the path is a directory. The name is taken, and a retry will not
    /// turn it into a file.
    pub const IS_A_DIRECTORY: i64 = -32006;

    /// `read`, `write`: the file could not be read or written (permissions, full disk,
    /// backend gone mid-operation).
    ///
    /// A failed `write` says nothing about how much of `data` landed; `read` to find out.
    pub const IO_FAILED: i64 = -32007;

    /// `init`: a mount whose scheme this server has no provider for; the message names
    /// the entry.
    ///
    /// Raised at `init` rather than at boot because supported schemes are a fact about the
    /// build, knowable from the frame alone. One code for all mounts, since the fix is the
    /// same: a different URL, or a build that has the provider.
    pub const UNSUPPORTED_MOUNT: i64 = -32008;

    /// `exec`, `read`, `write`: a tree could not be put where `init` said it would be
    /// (no mount binding compiled in, no FUSE provider, mount point busy, store
    /// unreachable).
    ///
    /// Mounting happens at boot, so this reaches the call that needed one. The session is
    /// described correctly; the environment is what has to change.
    pub const MOUNT_FAILED: i64 = -32009;

    /// `init`: a network reach this server cannot provide, named in the message.
    ///
    /// Raised at `init` because supported reaches are a fact about the build and machine.
    /// Covers both an unknown name and an understood one that cannot be honoured (a host
    /// backend cannot take the network away, so it refuses all but `full`); the fix is the
    /// same: ask for something else, or use another backend.
    ///
    /// A server never grants a different reach than the one named; it refuses instead.
    pub const UNSUPPORTED_NETWORK: i64 = -32010;

    /// `init`: this backend cannot swap the base image at all (its commands run on the
    /// server's own filesystem).
    ///
    /// An unfetchable reference is a failed boot instead.
    pub const UNSUPPORTED_IMAGE: i64 = -32011;

    /// `init`: the session named a built image this server does not have; the client can
    /// build it.
    ///
    /// Checked at `init` because it is a local file test; whether a registry has a
    /// reference is a network round trip, so an unfetchable reference is a failed boot.
    pub const UNKNOWN_IMAGE: i64 = -32012;

    /// `init`: this server cannot make the machine shape asked for (a GPU it has no device
    /// for, more vCPUs or memory than it will give).
    ///
    /// Raised at `init` because it depends only on the build and host, and a retry will not
    /// help: the request, or the server, has to change.
    ///
    /// A server never makes a smaller machine than asked; it refuses, since a client could
    /// otherwise only notice from how slow its commands are.
    pub const UNSUPPORTED_MACHINE: i64 = -32013;

    /// The spec codes a peer can hit here. `-32700` (parse error) belongs to the frame
    /// reader.
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

#[cfg(test)]
mod tests {
    use super::Error;

    /// Colliding codes would be indistinguishable failures, and the compiler cannot catch it.
    #[test]
    fn every_error_code_is_distinct() {
        let codes = [
            ("TIMED_OUT", Error::TIMED_OUT),
            ("NOT_EXECUTABLE", Error::NOT_EXECUTABLE),
            ("BOOT_FAILED", Error::BOOT_FAILED),
            ("NOT_FOUND", Error::NOT_FOUND),
            ("IS_A_DIRECTORY", Error::IS_A_DIRECTORY),
            ("IO_FAILED", Error::IO_FAILED),
            ("UNSUPPORTED_MOUNT", Error::UNSUPPORTED_MOUNT),
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
}
