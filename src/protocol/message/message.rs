//! The envelope: which of the three shapes an object is, and what puts it on the
//! wire.
//!
//! What is here is true of every message regardless of method — the `jsonrpc`
//! member, the `id` that pairs a response with its request, which of the three
//! shapes the member set makes it — and the serde impls that read and write all of
//! it.
//!
//! What a particular method carries is [`Call`]'s and [`Response`]'s — the request half
//! and the answering half — and each says what its own members are, so adding a method
//! means touching the side it belongs to and not the envelope.

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
/// Allocated by the client, which is the only end that asks, and counted from zero
/// by one. Nothing on the answering side reads a meaning into the number.
///
/// There is a single request outstanding at a time, so what the id earns is not
/// concurrency but certainty about what an answer answers: one carrying a number
/// nobody issued is a peer that has lost its place, and can be dropped rather than
/// mistaken for the answer that was due.
///
/// JSON-RPC also allows a string or null id. This protocol issues numbers, which
/// is what an off-the-shelf peer will happily accept; nothing reads any other
/// meaning into the value.
pub type RequestId = u64;

/// Refuses a frame length that could only be corruption or malice, rather than
/// allocating it and finding out.
///
/// Here rather than in the framing code because it is part of the contract: both
/// ends have to agree on it, or one will accept what the other would not send.
///
/// Since a response carries a whole execution's output, this is also the ceiling
/// on how much a command may write — see [`truncated`](super::ExecResp::truncated).
pub const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

/// One JSON-RPC object.
///
/// The three shapes the spec defines, told apart the way the spec tells them
/// apart: by which members are present. `method` with an `id` is a request,
/// `method` without one is a notification, and `result` or `error` is a response.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// `{"jsonrpc":"2.0","id":N,"method":..,"params":..}`
    ///
    /// Exactly one [`Response`](Message::Response) with the same `id` will answer
    /// it.
    Request { id: RequestId, call: Call },

    /// `{"jsonrpc":"2.0","method":..}` — no `id`, because nothing answers it.
    Notification(Notification),

    /// `{"jsonrpc":"2.0","id":N,"method":..,"result":..}` or `{..,"error":..}`
    ///
    /// `result` xor `error`, and both are [`Response`] — a response either is the answer
    /// or is the reason there is none, which is one question with one type. Nothing here
    /// wraps it in a second one to say which of the two it turned out to be.
    ///
    /// The `result` side is typed, which is what the echoed `method` is for; see
    /// [`Response`].
    Response { id: RequestId, result: Response },
}

impl Message {
    /// The request this message is, or answers. `None` for a notification, which
    /// is neither.
    pub fn id(&self) -> Option<RequestId> {
        match self {
            Message::Request { id, .. } | Message::Response { id, .. } => Some(*id),
            Message::Notification(_) => None,
        }
    }

    /// Which method this message concerns. `None` for a response, whose method is
    /// carried by the request its `id` came from and not on the wire — that is
    /// JSON-RPC's rule, and the reason an end has to remember what it asked.
    pub fn method(&self) -> Option<Method> {
        match self {
            Message::Request { call, .. } => Some(call.method()),
            Message::Notification(notification) => Some(notification.method()),
            Message::Response { .. } => None,
        }
    }
}

// A JSON-RPC object is not any one serde shape: the member set decides what it is,
// `params` and `result` are typed by the method rather than by position, and a
// notification is a request with a member missing. Deriving would mean choosing a
// tagging scheme the spec does not use, so the mapping is written out instead —
// which is also where `jsonrpc` gets checked, rather than being someone's job later.
//
// What a method carries is not decided here: a `Call` declares its own `params`, a
// `Notification` writes its own, and a `Response` says which of `result` and `error` it
// is. This half only knows which of the three shapes it is looking at, and — for a
// request or a response — hands that half an object already open so that its members
// land in this one. See [`flatten`](super::utils::flatten).
impl Serialize for Message {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // 4 is the largest a JSON-RPC object gets: jsonrpc, id, method, params.
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("jsonrpc", VERSION)?;

        match self {
            Message::Request { id, call } => {
                map.serialize_entry("id", id)?;
                // `method` and `params` are the call's own, and it writes them here
                // rather than under a member of its own — see [`flatten`](super::utils::flatten).
                call.serialize(FlatMapSerializer(&mut map))?;
            }

            Message::Notification(notification) => {
                map.serialize_entry("method", notification.method().as_str())?;
                notification.serialize_params(&mut map)?;
            }

            Message::Response { id, result } => {
                map.serialize_entry("id", id)?;
                // Which members those are — `method` and `result`, or `error` — is the
                // response's own business, the way a call's are, and they land here
                // rather than under a member of their own for the same reason — see
                // [`flatten`](super::utils::flatten).
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
        // `params`, `result` and `error` are held as values because member order is
        // not guaranteed: `method` may arrive after the `params` it types, and which
        // shape a message is cannot be known until every member has been seen.
        let mut params: Option<Bson> = None;
        let mut result: Option<Bson> = None;
        let mut error: Option<Bson> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "jsonrpc" => jsonrpc = Some(map.next_value()?),
                // A null id is legal JSON-RPC and is what an error response uses
                // when the request could not be read at all. Nothing here can
                // answer one, so it is taken and left as absent.
                "id" => id = map.next_value::<Option<RequestId>>()?,
                "method" => method = Some(map.next_value()?),
                "params" => params = Some(map.next_value()?),
                "result" => result = Some(map.next_value()?),
                "error" => error = Some(map.next_value()?),
                // Unknown members are ignored rather than refused: that is what
                // lets a peer add one without breaking us.
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

        // Which of the three shapes this is, is decided by the members that carry a
        // *payload* and not by `method`: a request and a response both name one now, and
        // what tells them apart is `params` against `result` xor `error`.
        if result.is_some() || error.is_some() {
            if params.is_some() {
                return Err(de::Error::custom(
                    "a message has params and a result or error, so it is neither \
                     a request nor a response",
                ));
            }
            let id = id.ok_or_else(|| de::Error::custom("a response needs an id"))?;
            // Which of `result` and `error` is there, and whether the one that is can be
            // read, is the response's own business — the way a call's `params` is a
            // call's. The three members go back to it as they arrived.
            let result = Response::from_members(method, result, error)?;
            return Ok(Message::Response { id, result });
        }

        // No result and no error, so a request or a notification — and an `id` is what
        // says which. `params` is typed by the method, which is why it waited.
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

    /// What a peer would have sent, and what it reads back as.
    ///
    /// Ids and error codes are `i64` on the wire because they are `u64` and `i64` in
    /// Rust and BSON has no unsigned type — so a `doc!` comparing against one writes
    /// `2i64`, not `2`, which would be an `Int32` and not equal.
    fn wire(message: &Message) -> Document {
        bson::serialize_to_document(message).unwrap()
    }

    fn read(doc: Document) -> Result<Message, bson::error::Error> {
        bson::deserialize_from_slice(&bson::serialize_to_vec(&doc).unwrap())
    }

    fn exec() -> ExecCall {
        ExecCall {
            // A multi-line `sh -c` script is one argument, and argv's last element
            // can be empty — both are ordinary argv.
            cmd: vec!["sh".into(), "-c".into(), "echo a\necho b".into(), "".into()],
            timeout_ms: Some(1_000),
        }
    }

    /// A whole session on one channel, in order: announce it, boot it, run something,
    /// release it, exit.
    ///
    /// Every request is the client's and every response the server's throughout, and the
    /// three notifications go unanswered because nothing answers one. That is the shape
    /// worth reading: an execution is one request and one response, however long the
    /// command took.
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
            // Optional, and nothing answers it: booting early rather than inside the
            // execution below.
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
                // A session that describes nothing, and waits forever, is a session too.
                Message::Request {
                    id: 6,
                    call: Call::Init(InitCall::default()),
                },
                Message::Response {
                    id: 6,
                    result: Response::Init(InitResp::default()),
                },
                // Booting is nothing's own request, so a backend that cannot come up
                // says so to whatever asked for the thing that needed one.
                Message::Response {
                    id: 1,
                    result: Response::Error(Error::new(Error::BOOT_FAILED, "no kvm")),
                },
                Message::Response {
                    id: 1,
                    result: Response::Error(Error::new(Error::TIMED_OUT, "killed after 1000ms")),
                },
                // The file plane. A read bounded on both ends, whose answer is shorter
                // than the file it came from.
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
                // And a whole-file write, which is the offset being absent.
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
                // Asked before anything else, and needing no session either.
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

    /// The whole reason for the manual impls: these are the members the spec calls
    /// for, member for member. BSON changes how they are spelled in bytes, not which
    /// of them are there.
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

        // An execution's ending is an ordinary `result`: the members of the ending itself,
        // with nothing wrapping them — beside the `method` that says which method's answer
        // they are, which is the whole of what a response carries that the spec's does not.
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

        // The file plane spells its bytes as `Binary` like everything else, and the
        // bounds it was given as plain members.
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

        // A notification has no id, and none of them takes parameters — so no `params`
        // either, which the spec allows leaving out and where `null` is not one of the
        // two types it permits.
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

        // And the session's one call, which is a request like any other.
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

    /// Output travels as bytes and not as text — the whole reason the codec is BSON.
    ///
    /// Asserted on the frame itself, because a round trip cannot tell the difference:
    /// base64 out and base64 back is symmetric. What proves it is that the payload's
    /// own bytes are *in* the frame, and its base64 spelling is not.
    #[test]
    fn output_travels_as_bytes_not_text() {
        // Not utf-8, and the byte a text framing would have had to escape.
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

    /// Member order is the sender's business, not ours — `params` may arrive
    /// before the `method` that types it. A BSON document keeps the order it was
    /// built in, so this really is out of order on the wire.
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

    /// Nothing a peer could send that is not one of the three shapes gets through
    /// as one of them.
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
        // And a notification cannot be answered, so it cannot ask to be — which is the
        // whole of what the `id` decides, for every one of the three.
        for name in ["start", "stop", "quit"] {
            refused(
                doc! {"jsonrpc": "2.0", "id": 1i64, "method": name},
                "cannot carry an id",
            );
        }
        // `result` xor `error`, and one of them.
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
        // A response names the method it answers, so `params` beside a `result` is a
        // message trying to be both.
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
        // A result with nothing to type it is one nothing can read.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "result": {"code": 0i32}},
            "needs the method it answers",
        );
        // A result that is not what the method answers with.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "exec", "result": Bson::Null},
            "exec result",
        );
        // And a method nothing answers cannot be answered.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "quit", "result": Bson::Null},
            "is not answered",
        );
        // Params that are not what the method takes.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "exec", "params": {"cmd": "ls"}},
            "exec params",
        );
        // An `exec`'s `cmd` is an argv and nothing else, so an object there is params
        // that are not what the method takes rather than a second shape to try.
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

    /// Every request in a session is answered exactly once, and only a
    /// notification goes unanswered.
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
