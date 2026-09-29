//! The envelope: what is true of every message regardless of method (`jsonrpc`, `id`,
//! which of the three shapes the member set makes it) and the serde impls for it.
//!
//! Per-method members belong to [`Call`] and [`Response`], so adding a method does not
//! touch the envelope.

use std::fmt;

use bson::Bson;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, MapAccess, Visitor},
    ser::SerializeMap,
};

use crate::protocol::message::{
    Call, Method, Notification, Response, utils::flatten::FlatMapSerializer,
};

/// The only `jsonrpc` member this protocol accepts.
pub const VERSION: &str = "2.0";

/// Pairs a response with the request it answers.
///
/// Allocated by the client, counting up from zero; the server attaches no meaning to it.
/// With one request outstanding, the id buys certainty rather than concurrency: an
/// answer carrying an unissued id is from a peer that lost its place, and is dropped.
///
/// JSON-RPC also allows string or null ids; this protocol issues only numbers.
pub type RequestId = u64;

/// Largest frame accepted; anything longer is treated as corruption or malice instead
/// of being allocated.
///
/// Part of the contract, not just framing: both ends must agree on it. It also caps a
/// command's output — see [`truncated`](super::ExecResp::truncated).
pub const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

/// One JSON-RPC object.
///
/// Told apart by which members are present: `method` with an `id` is a request,
/// `method` without one a notification, and `result` or `error` a response.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// `{"jsonrpc":"2.0","id":N,"method":..,"params":..}`
    ///
    /// Exactly one [`Response`](Message::Response) with the same `id` answers it.
    Request { id: RequestId, call: Call },

    /// `{"jsonrpc":"2.0","method":..}` — no `id`, because nothing answers it.
    Notification(Notification),

    /// `{"jsonrpc":"2.0","id":N,"method":..,"result":..}` or `{..,"error":..}`
    ///
    /// `result` xor `error`, both carried by one [`Response`]; the echoed `method`
    /// types the `result`.
    Response { id: RequestId, result: Response },
}

impl Message {
    /// The request this message is, or answers. `None` for a notification.
    pub fn id(&self) -> Option<RequestId> {
        match self {
            Message::Request { id, .. } | Message::Response { id, .. } => Some(*id),
            Message::Notification(_) => None,
        }
    }

    /// Which method this message concerns. `None` for a response; see
    /// [`Response::method`] for that.
    pub fn method(&self) -> Option<Method> {
        match self {
            Message::Request { call, .. } => Some(call.method()),
            Message::Notification(notification) => Some(notification.method()),
            Message::Response { .. } => None,
        }
    }
}

// Hand-written because the member set decides the shape, `params`/`result` are typed
// by the method, and a notification is a request missing `id`: no serde tagging scheme
// fits. It is also where `jsonrpc` is checked.
//
// Per-method members are written by `Call`, `Notification` and `Response` into the map
// opened here, so they land flat in this object. See [`flatten`](super::utils::flatten).
impl Serialize for Message {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("jsonrpc", VERSION)?;

        match self {
            Message::Request { id, call } => {
                map.serialize_entry("id", id)?;
                call.serialize(FlatMapSerializer(&mut map))?;
            }

            Message::Notification(notification) => {
                map.serialize_entry("method", notification.method().as_str())?;
                notification.serialize_params(&mut map)?;
            }

            Message::Response { id, result } => {
                map.serialize_entry("id", id)?;
                result.serialize(FlatMapSerializer(&mut map))?;
            }
        }

        map.end()
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Message, D::Error> {
        d.deserialize_map(MessageVisitor)
    }
}

struct MessageVisitor;

impl<'de> Visitor<'de> for MessageVisitor {
    type Value = Message;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON-RPC 2.0 request, notification or response")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Message, A::Error> {
        let mut jsonrpc: Option<String> = None;
        let mut id: Option<RequestId> = None;
        let mut method: Option<String> = None;
        // Held as values: member order is not guaranteed, so `method` may follow the
        // `params` it types, and the shape is known only after every member is seen.
        let mut params: Option<Bson> = None;
        let mut result: Option<Bson> = None;
        let mut error: Option<Bson> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "jsonrpc" => jsonrpc = Some(map.next_value()?),
                // A null id (legal, used when a request was unreadable) cannot be
                // answered, so it is treated as absent.
                "id" => id = map.next_value::<Option<RequestId>>()?,
                "method" => method = Some(map.next_value()?),
                "params" => params = Some(map.next_value()?),
                "result" => result = Some(map.next_value()?),
                "error" => error = Some(map.next_value()?),
                // Ignored, not refused, so a peer can add members without breaking us.
                _ => {
                    map.next_value::<de::IgnoredAny>()?;
                }
            }
        }

        match jsonrpc.as_deref() {
            Some(VERSION) => {}
            Some(other) => {
                return Err(de::Error::custom(format!(
                    "jsonrpc is {other:?}, not {VERSION:?}"
                )));
            }
            None => return Err(de::Error::missing_field("jsonrpc")),
        }

        let method = match method {
            Some(name) => Some(
                Method::parse(&name)
                    .ok_or_else(|| de::Error::custom(format!("unknown method {name:?}")))?,
            ),
            None => None,
        };

        // Shape is decided by the payload members, not `method`: requests and responses
        // both name a method, so `params` vs `result`/`error` is what tells them apart.
        if result.is_some() || error.is_some() {
            if params.is_some() {
                return Err(de::Error::custom(
                    "a message has params and a result or error, so it is neither \
                     a request nor a response",
                ));
            }
            let id = id.ok_or_else(|| de::Error::custom("a response needs an id"))?;
            // Validating `result` xor `error` and typing it is the response's job.
            let result = Response::from_members(method, result, error)?;
            return Ok(Message::Response { id, result });
        }

        // A request or a notification; the `id` says which.
        let Some(method) = method else {
            return Err(de::Error::custom(
                "a message has no method, result or error",
            ));
        };
        let params = params.unwrap_or(Bson::Null);

        match id {
            Some(id) => Ok(Message::Request {
                id,
                call: Call::from_params(method, params)?,
            }),
            None => Ok(Message::Notification(Notification::from_params(
                method, params,
            )?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use bson::{Document, doc};

    use super::{
        super::{
            BuildImageCall, BuildImageResp, Error, ExecCall, ExecResp, InitCall, InitResp,
            ListImagesCall, ListImagesResp, ReadCall, ReadResp, RemoveImageCall, RemoveImageResp,
            WriteCall, WriteResp,
        },
        *,
    };

    /// What a peer would have sent.
    ///
    /// Ids and codes are `Int64` on the wire (BSON has no unsigned type), so a `doc!`
    /// compared against one writes `2i64`; a bare `2` is an `Int32` and not equal.
    fn wire(message: &Message) -> Document {
        bson::serialize_to_document(message).unwrap()
    }

    fn read(doc: Document) -> Result<Message, bson::error::Error> {
        bson::deserialize_from_slice(&bson::serialize_to_vec(&doc).unwrap())
    }

    fn exec() -> ExecCall {
        ExecCall {
            // A multi-line script and an empty last element are both ordinary argv.
            cmd: vec!["sh".into(), "-c".into(), "echo a\necho b".into(), "".into()],
            timeout_ms: Some(1_000),
        }
    }

    /// A whole session on one channel, in order: describe, boot, run, release, exit.
    fn session() -> Vec<Message> {
        vec![
            Message::Request {
                id: 0,
                call: Call::Init(InitCall {
                    mounts: vec![
                        super::super::MountSpec::new("file:///srv/project", "/work")
                            .expect("a mount a test wrote")
                            .read_only(),
                    ],
                    ..InitCall::default()
                }),
            },
            Message::Response {
                id: 0,
                result: Response::Init(InitResp::default()),
            },
            // Optional and unanswered: boots before the exec rather than inside it.
            Message::Notification(Notification::Start),
            Message::Request {
                id: 1,
                call: Call::Exec(exec()),
            },
            Message::Response {
                id: 1,
                result: Response::Exec(ExecResp {
                    code: -1,
                    stdout: vec![0, 1, 2, 255, b'\n'],
                    stderr: vec![],
                    truncated: false,
                }),
            },
            Message::Notification(Notification::Stop),
            Message::Notification(Notification::Quit),
        ]
    }

    /// Everything a session above does not happen to contain.
    fn all() -> Vec<Message> {
        session()
            .into_iter()
            .chain([
                // An empty description is still a valid session.
                Message::Request {
                    id: 6,
                    call: Call::Init(InitCall::default()),
                },
                Message::Response {
                    id: 6,
                    result: Response::Init(InitResp::default()),
                },
                // A failed boot is reported to the call that needed it.
                Message::Response {
                    id: 1,
                    result: Response::Error(Error::new(Error::BOOT_FAILED, "no kvm")),
                },
                Message::Response {
                    id: 1,
                    result: Response::Error(Error::new(Error::TIMED_OUT, "killed after 1000ms")),
                },
                // A read bounded on both ends, whose answer is shorter than the file.
                Message::Request {
                    id: 7,
                    call: Call::Read(ReadCall {
                        path: "out/log.txt".into(),
                        offset: Some(4096),
                        len: Some(1024),
                    }),
                },
                Message::Response {
                    id: 7,
                    result: Response::Read(ReadResp {
                        data: vec![0xff, 0x00, b'\n'],
                        size: 10_000,
                    }),
                },
                Message::Response {
                    id: 7,
                    result: Response::Error(Error::new(Error::NOT_FOUND, "out/log.txt")),
                },
                // A whole-file write: no offset.
                Message::Request {
                    id: 8,
                    call: Call::Write(WriteCall {
                        path: "in/data".into(),
                        data: vec![0, 1, 2, 255],
                        offset: None,
                    }),
                },
                Message::Response {
                    id: 8,
                    result: Response::Write(WriteResp { size: 4 }),
                },
                // Needs no session.
                Message::Request {
                    id: 12,
                    call: Call::Version(super::super::VersionCall {}),
                },
                Message::Response {
                    id: 12,
                    result: Response::Version(super::super::VersionResp {
                        version: "0.1.0".into(),
                    }),
                },
                // The image plane, which needs no session.
                Message::Request {
                    id: 9,
                    call: Call::BuildImage(BuildImageCall {
                        recipe: crate::image::Recipe::new("alpine:3.20").step("apk add jq"),
                        reference: Some("myimg:latest".into()),
                    }),
                },
                Message::Response {
                    id: 9,
                    result: Response::BuildImage(BuildImageResp {
                        reference: "myimg:latest".into(),
                        digest: "sha256:0123abcd".into(),
                    }),
                },
                Message::Request {
                    id: 10,
                    call: Call::ListImages(ListImagesCall {}),
                },
                Message::Response {
                    id: 10,
                    result: Response::ListImages(ListImagesResp {
                        images: vec![crate::image::ImageEntry {
                            digest: "sha256:0123abcd".into(),
                            refs: vec!["myimg:latest".into()],
                        }],
                    }),
                },
                Message::Request {
                    id: 11,
                    call: Call::RemoveImage(RemoveImageCall {
                        image: crate::image::ImageSource::reference("myimg:latest"),
                    }),
                },
                Message::Response {
                    id: 11,
                    result: Response::RemoveImage(RemoveImageResp {}),
                },
            ])
            .collect()
    }

    #[test]
    fn messages_survive_a_roundtrip() {
        for message in all() {
            let doc = wire(&message);
            assert_eq!(read(doc.clone()).unwrap(), message, "{doc:?}");
        }
    }

    /// Exactly the members the spec calls for; BSON changes only their byte spelling.
    #[test]
    fn the_wire_is_json_rpc_2_0() {
        assert_eq!(
            wire(&Message::Request {
                id: 2,
                call: Call::Exec(ExecCall {
                    cmd: vec!["ls".into()],
                    ..ExecCall::default()
                }),
            }),
            doc! {"jsonrpc": "2.0", "id": 2i64, "method": "exec", "params": {"cmd": ["ls"]}},
        );

        assert_eq!(
            wire(&Message::Response {
                id: 2,
                result: Response::Error(Error::new(Error::TIMED_OUT, "killed after 1000ms")),
            }),
            doc! {
                "jsonrpc": "2.0",
                "id": 2i64,
                "error": {"code": -32000i64, "message": "killed after 1000ms"},
            },
        );

        // An unwrapped `result`, plus the echoed `method`: the only non-spec member.
        assert_eq!(
            wire(&Message::Response {
                id: 2,
                result: Response::Exec(ExecResp {
                    code: 0,
                    ..ExecResp::default()
                }),
            }),
            doc! {
                "jsonrpc": "2.0",
                "id": 2i64,
                "method": "exec",
                "result": {
                    "code": 0i32,
                    "stdout": Bson::Binary(bson::Binary {
                        subtype: bson::spec::BinarySubtype::Generic,
                        bytes: Vec::new(),
                    }),
                    "stderr": Bson::Binary(bson::Binary {
                        subtype: bson::spec::BinarySubtype::Generic,
                        bytes: Vec::new(),
                    }),
                    "truncated": false,
                },
            },
        );

        // File bytes are `Binary`; bounds are plain members.
        assert_eq!(
            wire(&Message::Request {
                id: 5,
                call: Call::Read(ReadCall {
                    path: "out/log.txt".into(),
                    offset: Some(4096),
                    len: Some(1024),
                }),
            }),
            doc! {
                "jsonrpc": "2.0",
                "id": 5i64,
                "method": "read",
                "params": {"path": "out/log.txt", "offset": 4096i64, "len": 1024i64},
            },
        );
        assert_eq!(
            wire(&Message::Request {
                id: 6,
                call: Call::Write(WriteCall {
                    path: "in/data".into(),
                    data: vec![0, 1, 2],
                    offset: None,
                }),
            }),
            doc! {
                "jsonrpc": "2.0",
                "id": 6i64,
                "method": "write",
                "params": {
                    "path": "in/data",
                    "data": Bson::Binary(bson::Binary {
                        subtype: bson::spec::BinarySubtype::Generic,
                        bytes: vec![0, 1, 2],
                    }),
                },
            },
        );

        // No id and no `params`: the spec allows omitting it, and `null` is not a
        // permitted type.
        assert_eq!(
            wire(&Message::Notification(Notification::Start)),
            doc! {"jsonrpc": "2.0", "method": "start"},
        );
        assert_eq!(
            wire(&Message::Notification(Notification::Stop)),
            doc! {"jsonrpc": "2.0", "method": "stop"},
        );
        assert_eq!(
            wire(&Message::Notification(Notification::Quit)),
            doc! {"jsonrpc": "2.0", "method": "quit"},
        );

        assert_eq!(
            wire(&Message::Request {
                id: 0,
                call: Call::Init(InitCall {
                    mounts: vec![
                        super::super::MountSpec::new("file:///srv/project", "/work")
                            .expect("a mount a test wrote")
                            .read_only(),
                    ],
                    ..InitCall::default()
                }),
            }),
            doc! {
                "jsonrpc": "2.0",
                "id": 0i64,
                "method": "init",
                "params": {"mounts": ["file:///srv/project:/work:ro"]},
            },
        );
    }

    /// Output travels as bytes, not base64.
    ///
    /// Checked on the raw frame, since a base64 round trip would be symmetric.
    #[test]
    fn output_travels_as_bytes_not_text() {
        // Not UTF-8, and includes a byte a text framing would escape.
        let payload = vec![0xff, 0xfe, 0x00, b'\n', 0x00];
        let message = Message::Response {
            id: 1,
            result: Response::Exec(ExecResp {
                code: 0,
                stdout: payload.clone(),
                ..ExecResp::default()
            }),
        };

        let frame = bson::serialize_to_vec(&wire(&message)).unwrap();
        assert!(
            frame.windows(payload.len()).any(|w| w == payload),
            "the payload is not in the frame verbatim: {frame:?}"
        );
        assert!(
            !frame.windows(8).any(|w| w == b"//4ACgA="),
            "the frame carries base64, so the byte type went unused"
        );

        assert_eq!(read(wire(&message)).unwrap(), message);
    }

    /// `params` may arrive before the `method` that types it. A BSON document keeps
    /// build order, so this is really out of order on the wire.
    #[test]
    fn members_may_arrive_in_any_order() {
        let doc = doc! {
            "params": {"cmd": ["ls"]},
            "id": 2i64,
            "method": "exec",
            "jsonrpc": "2.0",
        };
        let Message::Request {
            id: 2,
            call: Call::Exec(exec),
        } = read(doc).unwrap()
        else {
            panic!("wrong message")
        };
        assert_eq!(exec.cmd, vec!["ls".to_string()]);
    }

    /// Anything that is not one of the three shapes is rejected.
    #[test]
    fn malformed_objects_are_refused() {
        let refused = |doc: Document, because: &str| {
            let error = read(doc.clone())
                .expect_err(&format!("accepted {doc:?}"))
                .to_string();
            assert!(error.contains(because), "{doc:?} → {error}");
        };

        refused(doc! {"id": 1i64, "method": "exec"}, "jsonrpc");
        refused(
            doc! {"jsonrpc": "1.0", "id": 1i64, "method": "exec"},
            "not \"2.0\"",
        );
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "dance"},
            "unknown method",
        );
        // A request without an id has no way to be answered.
        refused(doc! {"jsonrpc": "2.0", "method": "exec"}, "needs an id");
        // A notification's method cannot carry an id.
        for name in ["start", "stop", "quit"] {
            refused(
                doc! {"jsonrpc": "2.0", "id": 1i64, "method": name},
                "cannot carry an id",
            );
        }
        // Exactly one of `result` and `error`.
        refused(
            doc! {
                "jsonrpc": "2.0",
                "id": 1i64,
                "result": Bson::Null,
                "error": {"code": 1i64, "message": "x"},
            },
            "both a result and an error",
        );
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64},
            "no method, result or error",
        );
        // `params` beside a `result` is trying to be both a request and a response.
        refused(
            doc! {
                "jsonrpc": "2.0",
                "id": 1i64,
                "method": "exec",
                "params": {"cmd": ["ls"]},
                "result": {"code": 0i32},
            },
            "neither a request nor a response",
        );
        // A result without a method cannot be typed.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "result": {"code": 0i32}},
            "needs the method it answers",
        );
        // A result that is not what the method answers with.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "exec", "result": Bson::Null},
            "exec result",
        );
        // A notification method cannot be answered.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "quit", "result": Bson::Null},
            "is not answered",
        );
        // Params that are not what the method takes.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "exec", "params": {"cmd": "ls"}},
            "exec params",
        );
        // An object as `cmd` is invalid params, not an alternative shape.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "exec", "params": {"cmd": {}}},
            "exec params",
        );
    }

    /// An unknown member is ignored, so a peer can add one without breaking us.
    #[test]
    fn unknown_members_are_ignored() {
        assert_eq!(
            read(doc! {
                "jsonrpc": "2.0",
                "method": "stop",
                "trace_id": "abc",
            })
            .unwrap(),
            Message::Notification(Notification::Stop),
        );
    }

    /// Every request is answered exactly once; only notifications go unanswered.
    #[test]
    fn a_session_pairs_every_request_with_one_response() {
        let mut outstanding: Vec<(RequestId, Method)> = Vec::new();

        for message in session() {
            match &message {
                Message::Notification(_) => {}

                Message::Request { id, call } => {
                    assert!(
                        !outstanding.iter().any(|(o, _)| o == id),
                        "{message:?} reuses a live id"
                    );
                    outstanding.push((*id, call.method()));
                }

                Message::Response { id, .. } => {
                    let at = outstanding
                        .iter()
                        .position(|(o, _)| o == id)
                        .unwrap_or_else(|| panic!("{message:?} answers a request nobody made"));
                    outstanding.remove(at);
                }
            }
        }

        assert!(
            outstanding.is_empty(),
            "requests left unanswered: {outstanding:?}"
        );
    }
}
