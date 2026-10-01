//! The methods nothing answers.
//!
//! [`Start`] and [`Stop`] are **resource management only**: a call that needs a booted
//! session boots one, so they change what the far end holds, never what a session can do.
//!
//! Each has a member-less type so every method has one place its params are written, and
//! adding a parameter later is a field rather than a new shape.

use bson::Bson;
use serde::{Deserialize, Serialize, de, ser::SerializeMap};

use super::Method;

/// A method with no `id`, so no response, error or result: for what holds whether or
/// not the other end acknowledges it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notification {
    /// **client → server.** Boot now, so that no command has to. See [`Start`].
    Start,

    /// **client → server.** Release what booting took. See [`Stop`].
    Stop,

    /// **client → server, last.** The session is over; exit. See [`Quit`].
    Quit,
}

impl Notification {
    pub fn method(&self) -> Method {
        match self {
            Notification::Start => Method::Start,
            Notification::Stop => Method::Stop,
            Notification::Quit => Method::Quit,
        }
    }

    /// Writes this notification's `params` into the object being serialized.
    ///
    /// None carry any; the exhaustive match forces a new notification to say what it sends.
    pub(super) fn serialize_params<M: SerializeMap>(&self, _map: &mut M) -> Result<(), M::Error> {
        match self {
            Notification::Start | Notification::Stop | Notification::Quit => Ok(()),
        }
    }

    /// The notification a `method` and its `params` name.
    ///
    /// Reached only for a message without an `id`, so a request's method here is refused:
    /// the peer asked for something and left no way to answer.
    pub(super) fn from_params<E: de::Error>(method: Method, _params: Bson) -> Result<Self, E> {
        match method {
            Method::Start => Ok(Notification::Start),
            Method::Stop => Ok(Notification::Stop),
            Method::Quit => Ok(Notification::Quit),
            _ => Err(E::custom(format!("{method} is a request and needs an id"))),
        }
    }
}

/// Boot now, so that no command has to. The `params` of `start`, which are none.
///
/// Hides the cold start: every call that needs a booted session boots on demand, so this
/// only lets the boot overlap with the client's other work instead of the first command's
/// latency.
///
/// Unanswered because a failed boot and a not-yet-attempted one are the same session: the
/// next call that needs a boot retries and gets [`BOOT_FAILED`](super::Error::BOOT_FAILED).
///
/// No-op on an already-booted session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start;

/// Release what booting took. The `params` of `stop`, which are none.
///
/// The guest goes away, the tree is unmounted, and the scratch directory is cleaned up
/// by whoever made it. Worth sending before a long idle stretch, not between commands.
///
/// Optional and reversible: the next call that needs a booted session boots again under
/// the same [`InitCall`](super::InitCall), so the only cost is that boot. A stopped
/// session is still a session; ending the process is [`Quit`].
///
/// Does not cancel an execution; that is [`timeout_ms`](super::ExecCall::timeout_ms)'s job.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stop;

/// The session is over; exit. The `params` of `quit`, which are none.
///
/// Unanswered, since a closed channel says the rest; sending it lets the other end tell
/// a finished session from a dead peer. The server also releases what a [`Stop`] would,
/// so ending takes one message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quit;
