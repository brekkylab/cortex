//! What each method carries, one file per method.
//!
//! A method is the unit here: [`Method`] names it, and the file named after it holds
//! everything that method sends and everything it answers with — `exec` has [`Exec`] and
//! its [`ExecResult`], `read` has [`Read`] and its [`ReadResult`]. Which is the split
//! that matters for changing this protocol: a method is one thing to read and one thing
//! to touch. Whether something is answered is [`Call`](super::Call)'s and
//! [`Notification`](super::Notification)'s business, and they name these types rather
//! than defining them.
//!
//! A method that carries nothing still gets a type, so that giving one a member later is
//! a field rather than a shape that did not exist. [`Start`], [`Stop`] and [`Quit`]
//! share `notifications.rs` for that reason and against the rule above: a name and a
//! paragraph each is not a file each, and what they have to say is mostly about one
//! another.

mod exec;
mod init;
mod notifications;
mod read;
mod write;

use std::fmt;

use bson::Bson;
use serde::de::{self, DeserializeOwned};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};

pub use exec::*;
pub use init::*;
pub use notifications::*;
pub use read::*;
pub use write::*;

/// Which method a request called, and its response answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Init,
    Exec,
    Read,
    Write,
    Start,
    Stop,
    Quit,
}

impl Method {
    /// The name as it appears in a `method` member.
    pub fn as_str(&self) -> &'static str {
        match self {
            Method::Init => "init",
            Method::Exec => "exec",
            Method::Read => "read",
            Method::Write => "write",
            Method::Start => "start",
            Method::Stop => "stop",
            Method::Quit => "quit",
        }
    }

    pub fn parse(name: &str) -> Option<Method> {
        Some(match name {
            "init" => Method::Init,
            "exec" => Method::Exec,
            "read" => Method::Read,
            "write" => Method::Write,
            "start" => Method::Start,
            "stop" => Method::Stop,
            "quit" => Method::Quit,
            _ => return None,
        })
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A method and its parameters.
///
/// All of them are the client's: the client asks and the server answers, and that is
/// true of every method here. Nothing the server has to say arrives as a request of
/// its own — see [`Progress`] for how a server that needs something from the client
/// asks for it while remaining the answering end.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    /// This is the session: the names it may call back into, and the tree it works in.
    ///
    /// The one exchange that is about the session rather than about work, and the one
    /// thing worth answering about it — because the answer is something a client can
    /// act on before it has asked for anything. A channel that answers this has a
    /// server on the far end that read the frame, speaks this protocol, and has taken
    /// what it was told; a notification could say none of that. The answer also carries
    /// where the volume went, which is what every later path in the session is spelled
    /// in — see [`InitResult`].
    ///
    /// It carries no booting, and no mounting either. Bringing a backend up costs a kernel
    /// on one and nothing at all on another, and a session's shape is the same either way,
    /// so *when* to pay for it is [`Start`](super::Notification::Start)'s and not this
    /// method's. The tree is put where this said it would be at the same moment.
    ///
    /// A second one replaces the first, and takes whatever was booted under it with it:
    /// the delegated names and the tree are both built into what booting produced, so a
    /// session that changes either has a boot that no longer matches it.
    Init(Init),

    /// Run this command.
    Exec(Exec),

    /// Hand back part of a file.
    Read(Read),

    /// Put these bytes in a file.
    Write(Write),
}

impl Call {
    pub fn method(&self) -> Method {
        match self {
            Call::Init(_) => Method::Init,
            Call::Exec(_) => Method::Exec,
            Call::Read(_) => Method::Read,
            Call::Write(_) => Method::Write,
        }
    }

    /// Writes this call's `params` into the object being serialized.
    ///
    /// Here rather than in [`Message`](super::Message)'s serializer because what a
    /// method's parameters are is this type's business, and a variant added later
    /// should have one place to say so. Every call takes some today; one that takes
    /// none would omit `params` rather than write null, since the spec says it may be
    /// left out and `null` is not one of the two types it allows.
    pub(super) fn serialize_params<M: SerializeMap>(&self, map: &mut M) -> Result<(), M::Error> {
        match self {
            Call::Init(init) => map.serialize_entry("params", init),
            Call::Exec(exec) => map.serialize_entry("params", exec),
            Call::Read(read) => map.serialize_entry("params", read),
            Call::Write(write) => map.serialize_entry("params", write),
        }
    }

    /// The call a `method` and its `params` name.
    ///
    /// Reached only for a message that carried an `id`, which is what says it is a
    /// request at all — so a notification's method arriving here is a peer asking
    /// to be answered about something nothing answers.
    pub(super) fn from_params<E: de::Error>(method: Method, params: Bson) -> Result<Self, E> {
        Ok(match method {
            Method::Init => Call::Init(typed_params(method, params)?),
            Method::Exec => Call::Exec(typed_params(method, params)?),
            Method::Read => Call::Read(typed_params(method, params)?),
            Method::Write => Call::Write(typed_params(method, params)?),
            Method::Start | Method::Stop | Method::Quit => {
                return Err(E::custom(format!(
                    "{method} is a notification and cannot carry an id"
                )));
            }
        })
    }
}

/// A method's `params`, as the type that method takes.
///
/// Generic because every method takes a different shape, and the error names the method
/// so a rejection says which one was expected.
fn typed_params<T: DeserializeOwned, E: de::Error>(method: Method, params: Bson) -> Result<T, E> {
    bson::deserialize_from_bson(params).map_err(|e| E::custom(format!("{method} params: {e}")))
}

/// A method that is not answered.
///
/// A notification is a method with no `id`, and so no response, no error and no result
/// — which makes it the right shape for exactly one kind of thing: what is true whether
/// or not the other end acknowledges it. All three here are that, and `notifications.rs`
/// argues each of them.
///
/// Which side a method is on is what JSON-RPC's `id` decides, so it is a decision a
/// method has to make rather than inherit: [`Call`] is the other one.
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
    /// Nothing carries any today, and the match is what makes that a decision
    /// rather than an omission: a notification added later cannot compile without
    /// saying what it sends.
    pub(super) fn serialize_params<M: SerializeMap>(&self, _map: &mut M) -> Result<(), M::Error> {
        match self {
            Notification::Start | Notification::Stop | Notification::Quit => Ok(()),
        }
    }

    /// The notification a `method` and its `params` name.
    ///
    /// Reached only for a message that carried no `id`, which is what says nothing
    /// will answer it — so a request's method arriving here is a peer that has
    /// asked for something and left no way to be told.
    pub(super) fn from_params<E: de::Error>(method: Method, _params: Bson) -> Result<Self, E> {
        match method {
            Method::Start => Ok(Notification::Start),
            Method::Stop => Ok(Notification::Stop),
            Method::Quit => Ok(Notification::Quit),
            _ => Err(E::custom(format!("{method} is a request and needs an id"))),
        }
    }
}

/// Raw bytes, as themselves.
///
/// BSON has a byte type — `Binary`, subtype `Generic` — so the bulk of what this channel
/// carries goes across at 1.0× rather than as text. This is the whole reason the codec is
/// BSON and not JSON: JSON has no byte type, so `stdout` had to be base64 (1.37×) to
/// avoid being an array of numbers (`[104,105,10]`, 4×).
///
/// Here rather than in one method's file because three of them carry bytes — an
/// [`ExecResult`]'s output, a [`ReadResult`]'s data, a [`Write`]'s — and how bytes reach
/// the wire is the codec's business and not any one method's.
///
/// # Why this does not ask the codec
///
/// [`is_human_readable`](serde::Serializer::is_human_readable) is the obvious way for one
/// helper to serve a textual codec and a binary one, and it cannot be used here, because
/// it silently loses. A message's `params` and `result` are held as a
/// [`Bson`](bson::Bson) before they reach the wire — they have to be, since a `result` is
/// typed by a method only the caller knows — and `bson`'s *value-level* serializer
/// reports `is_human_readable() == true`. So the branch would encode base64 on the way
/// into the `Bson`, and the wire would faithfully carry a string: 1.37×, the byte type
/// unused, and nothing to show it had happened. `bson`'s `SerializerOptions` is
/// `pub(crate)`, so it cannot be told otherwise.
///
/// One codec, and it has bytes. If a textual wire is ever wanted for a person to read,
/// BSON's own projection is the thing to reach for — `Bson::into_relaxed_extjson` spells
/// `Binary` as `{"$binary": ..}` — rather than a second spelling in here.
pub(super) mod bytes {
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
