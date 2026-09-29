//! The methods that are answered, and what each of them asks for.
//!
//! [`Call`] is which one a request names; the rest is what each one carries — an
//! [`InitCall`] and the vocabulary a session is described in, an [`ExecCall`], a
//! [`ReadCall`], a [`WriteCall`].
//!
//! One file for the asking half and one for the answering half, because that is the
//! division a reader of this protocol has: a client writes calls and reads responses, a
//! server does the reverse, and neither is ever holding both halves of a method at once.
//! It is also the division the envelope names — [`Call`] and [`Response`](super::Response)
//! are the two types a [`Message`](super::Message) is built from — so a file holds exactly
//! what one of those enums can be, and adding a method is one thing to add on each side.
//!
//! The three methods that are answered by nothing are `notification`'s, next door.
mod build_image;
mod exec;
mod init;
mod list_images;
mod read;
mod remove_image;
mod snapshot;
mod version;
mod write;

use std::fmt;

use bson::{Bson, Document, doc};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, DeserializeOwned, MapAccess, Visitor},
    ser::SerializeMap,
};

pub use build_image::{BuildImageCall, BuildImageResp};
pub use exec::{ExecCall, ExecResp};
pub use init::{InitCall, InitResp, InvalidMount, InvalidPort, MountSpec, Port};
pub use list_images::{ListImagesCall, ListImagesResp};
pub use read::{ReadCall, ReadResp};
pub use remove_image::{RemoveImageCall, RemoveImageResp};
pub use snapshot::{SnapshotCall, SnapshotResp};
pub use version::{VersionCall, VersionResp};
pub use write::{WriteCall, WriteResp};

use crate::protocol::Error;

use super::Method;

/// A method and its parameters: what a request carries.
///
/// The requests as one type, each variant holding what its method takes.
///
/// # On the wire
///
/// Part of a JSON-RPC request: the `method` and `params` fields.
///
/// What serde writes for it, spelled as BSON's Extended JSON would show it — with
/// `<Binary>` standing in for a byte payload, which BSON carries as `Binary` and
/// JSON has no spelling for:
///
/// ```text
/// {"method":"init","params":{"mounts":["file:///srv/project:/work:ro"]}}
/// {"method":"exec","params":{"cmd":["sh","-c","ls"],"timeout_ms":1000}}
/// {"method":"read","params":{"path":"out/log.txt","offset":4096,"len":1024}}
/// {"method":"write","params":{"path":"in/data","data":<Binary>}}
/// {"method":"version","params":{}}
/// {"method":"build_image","params":{"recipe":{"v":1,"base":"alpine:3.20","steps":[]},"ref":"myimg:latest"}}
/// {"method":"remove_image","params":{"image":{"type":"ref","ref":"myimg:latest"}}}
/// {"method":"list_images","params":{}}
/// ```
///
/// An optional member that was not set is left out rather than sent as null — the
/// `write` above carries no `offset`, which is what asks for a whole-file write.
///
/// `jsonrpc` and `id` are [`Message`](super::Message)'s, and so is putting these two
/// members beside them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Call {
    /// Which protocol version the server speaks.
    ///
    /// Not part of a session, like [`BuildImage`](Self::BuildImage).
    Version(VersionCall),

    /// Build this recipe, and store it under a ref if one is given.
    ///
    /// Not part of a session, so it may be sent before an `init`, after one, or on a channel
    /// that never has one.
    BuildImage(BuildImageCall),

    /// Forget a built image.
    ///
    /// Not part of a session, like [`BuildImage`](Self::BuildImage).
    RemoveImage(RemoveImageCall),

    /// Every image this server has built.
    ///
    /// Not part of a session, like [`BuildImage`](Self::BuildImage).
    ListImages(ListImagesCall),

    /// This is the session: the trees it works in, and what its commands run in and may reach.
    ///
    /// The one exchange that is about the session rather than about work, and the one
    /// thing worth answering about it — because the answer is something a client can
    /// act on before it has asked for anything. A channel that answers this has a
    /// server on the far end that read the frame, speaks this protocol, and has taken
    /// what it was told; a notification could say none of that. The answer also says where
    /// the session stands to begin with, which is the one thing about it a client could not
    /// have worked out from what it sent — see [`InitResp`](super::InitResp).
    ///
    /// It carries no booting, and no mounting either. Bringing a backend up costs a kernel
    /// on one and nothing at all on another, and a session's shape is the same either way,
    /// so *when* to pay for it is [`Start`](super::Notification::Start)'s and not this
    /// method's. Each tree is put where this said it would be at the same moment.
    ///
    /// A second one replaces the first, and takes whatever was booted under it with it: the
    /// trees are built into what booting produced, so a session that changes them has a boot
    /// that no longer matches it.
    Init(InitCall),

    /// Run this command.
    Exec(ExecCall),

    /// Hand back part of a file.
    Read(ReadCall),

    /// Put these bytes in a file.
    Write(WriteCall),

    /// Take what this session has written so far, as a blob a later session can start from.
    Snapshot(SnapshotCall),
}

impl Call {
    pub fn method(&self) -> Method {
        match self {
            Call::Version(_) => Method::Version,
            Call::BuildImage(_) => Method::BuildImage,
            Call::RemoveImage(_) => Method::RemoveImage,
            Call::ListImages(_) => Method::ListImages,
            Call::Init(_) => Method::Init,
            Call::Exec(_) => Method::Exec,
            Call::Read(_) => Method::Read,
            Call::Write(_) => Method::Write,
            Call::Snapshot(_) => Method::Snapshot,
        }
    }

    /// The call a `method` and its `params` name.
    ///
    /// Reached only for a message that carried an `id`, which is what says it is a
    /// request at all — so a notification's method arriving here is a peer asking to be
    /// answered about something nothing answers, and is refused as that rather than as
    /// a name this enum happens not to have.
    ///
    /// The two members are put back together into the object the derive above expects,
    /// because the envelope read them out of a flat one and had to: `params` may arrive
    /// before the `method` that types it. Nothing is copied — `params` is moved in as
    /// the value it already was.
    pub(super) fn from_params<E: de::Error>(method: Method, params: Bson) -> Result<Self, E> {
        if method.is_notification() {
            return Err(E::custom(format!(
                "{method} is a notification and cannot carry an id"
            )));
        }

        let mut object = doc! { "method": method.as_str() };
        // An absent `params` becomes an empty one rather than staying absent: the derive
        // above is adjacently tagged, so serde wants the member present whatever the
        // method takes, and a missing one is `missing field \`params\`` and not a method
        // read as taking none. What an empty object is short of is then the method's own
        // to refuse — `init` takes it, `exec` wants a `cmd`.
        object.insert(
            "params",
            match params {
                Bson::Null => Bson::Document(bson::Document::new()),
                params => params,
            },
        );

        bson::deserialize_from_bson(Bson::Document(object))
            .map_err(|e| E::custom(format!("{method} params: {e}")))
    }
}

/// What a request was answered with: the method's own result, or why there is none.
///
/// The mirror of [`Call`](super::Call), and the whole of what a response can be. A `Call` is what a
/// request carries under `params`; this is what its response carries — under `result` when
/// the method produced one, and under `error` when it did not.
///
/// # On the wire
///
/// Part of a JSON-RPC response: the `method` and `result` members, or the `error` that
/// stands where a result would have been.
///
/// What serde writes for it, spelled as BSON's Extended JSON would show it — with
/// `<Binary>` standing in for a byte payload:
///
/// ```text
/// {"method":"init","result":{"cwd":"/work"}}
/// {"method":"exec","result":{"code":0,"stdout":<Binary>,"stderr":<Binary>,"truncated":false}}
/// {"method":"read","result":{"data":<Binary>,"size":4096}}
/// {"error":{"code":-32000,"message":"timed out after 1000ms"}}
/// ```
///
/// `jsonrpc` and `id` are [`Message`](super::Message)'s, and so is putting these members
/// beside them.
///
/// # One place an error can be
///
/// [`Error`] is a variant here and not a second channel wrapped around this type. A
/// response either *is* the answer or *is* the reason there is none, which is one question
/// with one place to look — where an `Err` around the four below would have made "which
/// kind of failure is this" a thing every reader asks before it can ask anything else.
///
/// So a server hands back one value, a client matches one value, and the codes on
/// [`Error`] are the whole of what a requester branches on.
///
/// # Why a response carries its `method`
///
/// JSON-RPC does not put one there: an `id` identifies a response, and the caller is
/// expected to remember what it asked. That works for a caller and not for a *reader* —
/// `{"code":0,…}` and `{"size":10}` are both objects, and nothing in the bytes says which
/// method's answer either one is.
///
/// So a response echoes the `method` of the request it answers, and this type is what that
/// buys: one frame, self-describing, read the same way by whoever is holding it. The
/// alternative was for the reading end to keep an id→method table and hand the method to a
/// parser that is otherwise a free function — state in one of the two places this protocol
/// has none.
///
/// It is a departure from the spec and a small one. `result` is still exactly the method's
/// own payload with nothing wrapped around it, so a peer that ignores the extra member
/// reads what it always read. [`Error`] needs none, because an error is one type whichever
/// method it answers — which is why the variant below carries no method either, and why
/// the impls further down write `method` beside `result` and never beside `error`.
#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    Version(VersionResp),
    BuildImage(BuildImageResp),
    RemoveImage(RemoveImageResp),
    ListImages(ListImagesResp),
    Init(InitResp),
    Exec(ExecResp),
    Read(ReadResp),
    Write(WriteResp),
    Snapshot(SnapshotResp),

    /// Why the method produced no result. See [`Error`] for the codes.
    Error(Error),
}

impl Response {
    /// Which method this answers, or `None` for an [`Error`](Response::Error) — which
    /// answers whichever one asked, and says so with a code rather than a name.
    pub fn method(&self) -> Option<Method> {
        Some(match self {
            Response::Version(_) => Method::Version,
            Response::BuildImage(_) => Method::BuildImage,
            Response::RemoveImage(_) => Method::RemoveImage,
            Response::ListImages(_) => Method::ListImages,
            Response::Snapshot(_) => Method::Snapshot,
            Response::Init(_) => Method::Init,
            Response::Exec(_) => Method::Exec,
            Response::Read(_) => Method::Read,
            Response::Write(_) => Method::Write,
            Response::Error(_) => return None,
        })
    }

    /// The error, if this is one.
    pub fn error(&self) -> Option<&Error> {
        match self {
            Response::Error(error) => Some(error),
            _ => None,
        }
    }

    /// The response a set of members names.
    ///
    /// [`Call::from_params`](super::Call::from_params)'s twin, and here for the same
    /// reason: the envelope read these out of a *flat* object and had to, because member
    /// order is not guaranteed and a `result` is typed by a `method` that may arrive after
    /// it. Putting them back together is what lets the impl below be the one place a
    /// response's shape is written down, rather than half of it living in the envelope's
    /// visitor. Nothing is copied — each member is moved in as the value it already was.
    ///
    /// All three are optional because that is how they reached the envelope. Which
    /// combinations are a response and which are not is decided below, with everything
    /// else about the shape.
    pub(super) fn from_members<E: de::Error>(
        method: Option<Method>,
        result: Option<Bson>,
        error: Option<Bson>,
    ) -> Result<Self, E> {
        let mut object = Document::new();
        if let Some(method) = method {
            object.insert("method", method.as_str());
        }
        if let Some(result) = result {
            object.insert("result", result);
        }
        if let Some(error) = error {
            object.insert("error", error);
        }

        bson::deserialize_from_bson(Bson::Document(object)).map_err(E::custom)
    }
}

// A response is not a shape serde derives: `method` is a tag for four of the variants and
// absent for the fifth, and which of `result` and `error` is *present* is what decides
// between them. Adjacent tagging would get the first four and has nowhere to put the
// error; untagged would get both and would report a mistyped result as "no variant
// matched" rather than as the method's own payload being wrong. So the mapping is written
// out — which is also where `result` xor `error` is enforced.
//
// Both halves work in members rather than in an object of their own, because a JSON-RPC
// object is flat: the serializer writes into a map the envelope opened (see
// `message::flatten`), and the deserializer reads the members the envelope handed back.

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // 2 is the largest a response's half of the object gets: method and result.
        let mut map = s.serialize_map(None)?;

        // Beside `result` and never beside `error`, which is the whole of the asymmetry:
        // a result needs the method to be typed and an error never did.
        if let Some(method) = self.method() {
            map.serialize_entry("method", method.as_str())?;
        }
        match self {
            Response::Version(version) => map.serialize_entry("result", version)?,
            Response::BuildImage(build) => map.serialize_entry("result", build)?,
            Response::RemoveImage(remove) => map.serialize_entry("result", remove)?,
            Response::ListImages(list) => map.serialize_entry("result", list)?,
            Response::Init(init) => map.serialize_entry("result", init)?,
            Response::Exec(exec) => map.serialize_entry("result", exec)?,
            Response::Read(read) => map.serialize_entry("result", read)?,
            Response::Write(write) => map.serialize_entry("result", write)?,
            Response::Snapshot(snapshot) => map.serialize_entry("result", snapshot)?,
            Response::Error(error) => map.serialize_entry("error", error)?,
        }

        map.end()
    }
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Response, D::Error> {
        d.deserialize_map(ResponseVisitor)
    }
}

struct ResponseVisitor;

impl<'de> Visitor<'de> for ResponseVisitor {
    type Value = Response;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON-RPC 2.0 response's result or error")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Response, A::Error> {
        let mut method: Option<Method> = None;
        // `result` is held as a value because member order is not guaranteed: it may
        // arrive before the `method` that types it.
        let mut result: Option<Bson> = None;
        let mut error: Option<Error> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "method" => {
                    let name = map.next_value::<String>()?;
                    method =
                        Some(Method::parse(&name).ok_or_else(|| {
                            de::Error::custom(format!("unknown method {name:?}"))
                        })?);
                }
                "result" => result = Some(map.next_value()?),
                "error" => error = Some(map.next_value()?),
                // Unknown members are ignored rather than refused: that is what lets a
                // peer add one without breaking us.
                _ => {
                    map.next_value::<de::IgnoredAny>()?;
                }
            }
        }

        match (result, error) {
            // A `result` is typed by the method the response echoed, and one that names
            // none is an answer nothing can read.
            (Some(result), None) => {
                let method = method
                    .ok_or_else(|| de::Error::custom("a result needs the method it answers"))?;
                typed_result(method, result)
            }
            (None, Some(error)) => Ok(Response::Error(error)),
            (Some(_), Some(_)) => Err(de::Error::custom("both a result and an error")),
            (None, None) => Err(de::Error::custom("neither a result nor an error")),
        }
    }
}

/// The response a `method` and its `result` name.
///
/// An `error` does not come through here and needs nothing typed: it is one type whichever
/// method it answers.
fn typed_result<E: de::Error>(method: Method, result: Bson) -> Result<Response, E> {
    Ok(match method {
        Method::Version => Response::Version(payload(method, result)?),
        Method::BuildImage => Response::BuildImage(payload(method, result)?),
        Method::RemoveImage => Response::RemoveImage(payload(method, result)?),
        Method::ListImages => Response::ListImages(payload(method, result)?),
        Method::Snapshot => Response::Snapshot(payload(method, result)?),
        Method::Init => Response::Init(payload(method, result)?),
        Method::Exec => Response::Exec(payload(method, result)?),
        Method::Read => Response::Read(payload(method, result)?),
        Method::Write => Response::Write(payload(method, result)?),
        Method::Start | Method::Stop | Method::Quit => {
            return Err(E::custom(format!(
                "{method} is a notification and is not answered"
            )));
        }
    })
}

/// A method's `result`, as the type that method answers with.
///
/// Apart from the dispatch above so that a rejection says which method's answer was
/// unreadable, rather than only that some member of some object was.
fn payload<T: DeserializeOwned, E: de::Error>(method: Method, result: Bson) -> Result<T, E> {
    bson::deserialize_from_bson(result).map_err(|e| E::custom(format!("{method} result: {e}")))
}
