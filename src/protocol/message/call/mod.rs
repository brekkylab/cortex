//! The answered methods: [`Call`] (what a request carries) and [`Response`] (what
//! answers it), with one submodule per method for its params and result types.
//!
//! Unanswered methods are in `notification`.
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
pub use init::{InitCall, InitResp, InvalidMount, MountSpec, NetworkAccess};
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
/// # On the wire
///
/// Part of a JSON-RPC request: the `method` and `params` fields.
///
/// As BSON Extended JSON, with `<Binary>` for a byte payload:
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
/// An unset optional member is omitted, not sent as null; the `write` above has no
/// `offset`, which asks for a whole-file write.
///
/// `jsonrpc` and `id` are added by [`Message`](super::Message).
///
/// `Version`, `BuildImage`, `RemoveImage` and `ListImages` are not part of a session: they
/// may be sent before an `init`, after one, or on a channel that never has one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Call {
    /// Which protocol version the server speaks.
    Version(VersionCall),

    /// Build this recipe, and store it under a ref if one is given.
    BuildImage(BuildImageCall),

    /// Forget a built image.
    RemoveImage(RemoveImageCall),

    /// Every image this server has built.
    ListImages(ListImagesCall),

    /// This is the session: the trees it works in, and what its commands run in and may reach.
    ///
    /// Answered, unlike a notification, because the answer tells the client a server read
    /// the frame, speaks this protocol and accepted the description, plus where the session
    /// starts (see [`InitResp`](super::InitResp)), which the client could not work out alone.
    ///
    /// Boots and mounts nothing: boot cost varies by backend, so *when* to pay it is
    /// [`Start`](super::Notification::Start)'s. Trees are mounted at boot.
    ///
    /// A second `init` replaces the first and tears down whatever was booted under it,
    /// since the trees are built into the boot.
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
    /// Reached only for a message with an `id`, so a notification's method is refused as
    /// such rather than as an unknown name.
    ///
    /// Reassembles the adjacently tagged object the derive expects, since the envelope
    /// had to read the members flat (`params` may precede `method`). `params` is moved,
    /// not copied.
    pub(super) fn from_params<E: de::Error>(method: Method, params: Bson) -> Result<Self, E> {
        if method.is_notification() {
            return Err(E::custom(format!(
                "{method} is a notification and cannot carry an id"
            )));
        }

        let mut object = doc! { "method": method.as_str() };
        // Absent `params` becomes `{}`: adjacent tagging requires the member, and an
        // empty object lets each method decide (`init` accepts it, `exec` needs `cmd`).
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
/// # On the wire
///
/// Part of a JSON-RPC response: `method` and `result`, or `error` alone. As BSON
/// Extended JSON, with `<Binary>` for a byte payload:
///
/// ```text
/// {"method":"init","result":{"cwd":"/work"}}
/// {"method":"exec","result":{"code":0,"stdout":<Binary>,"stderr":<Binary>,"truncated":false}}
/// {"method":"read","result":{"data":<Binary>,"size":4096}}
/// {"error":{"code":-32000,"message":"timed out after 1000ms"}}
/// ```
///
/// `jsonrpc` and `id` are added by [`Message`](super::Message).
///
/// [`Error`] is a variant rather than an `Err` around this type, so a server returns one
/// value, a client matches one value, and [`Error`]'s codes are all a requester branches on.
///
/// # Why a response carries its `method`
///
/// JSON-RPC expects the caller to remember what it asked by `id`, but `{"code":0,…}` and
/// `{"size":10}` are indistinguishable to a reader. Echoing `method` makes each frame
/// self-describing, so no end needs an id→method table. It is a small departure from the
/// spec: `result` is still the bare payload, so a peer ignoring `method` reads it as
/// before. An error is one type for every method, so `method` is written beside `result`
/// and never beside `error`.
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
    /// Which method this answers; `None` for an [`Error`](Response::Error), which carries
    /// no method.
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
    /// The envelope reads members flat (a `result` may precede the `method` that types
    /// it); reassembling them keeps the `Deserialize` impl below the one place a response's
    /// shape, including which member combinations are valid, is decided. Members are
    /// moved, not copied.
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

// Hand-written: `method` tags every variant but `Error`, and member presence picks
// `result` vs `error`. Adjacent tagging has nowhere for the error; untagged would report a
// mistyped result as "no variant matched". This is also where `result` xor `error` is
// enforced.
//
// Both halves work in members, not a nested object: serialization writes into the map
// the envelope opened (see `utils::flatten`), and deserialization reads the members the
// envelope handed back.

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;

        // Only beside `result`: an error needs no method to be typed.
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
        // Held as a value: it may arrive before the `method` that types it.
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
                // Ignored, not refused, so a peer can add members without breaking us.
                _ => {
                    map.next_value::<de::IgnoredAny>()?;
                }
            }
        }

        match (result, error) {
            // A `result` without an echoed method cannot be typed.
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
/// Separate so a rejection names the method whose answer was unreadable.
fn payload<T: DeserializeOwned, E: de::Error>(method: Method, result: Bson) -> Result<T, E> {
    bson::deserialize_from_bson(result).map_err(|e| E::custom(format!("{method} result: {e}")))
}
