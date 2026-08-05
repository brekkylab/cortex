//! The envelope: which of the three shapes an object is, and what puts it on the
//! wire.
//!
//! What is here is true of every message regardless of method — the `jsonrpc`
//! member, the `id` that pairs a response with its request, which of the three
//! shapes the member set makes it — and the serde impls that read and write all of
//! it.
//!
//! What a particular method carries is [`Call`]'s and [`Notification`]'s, and how
//! something *ended* is [`Outcome`]'s: a response carries one, and so does a
//! `resume`, which is why it is not in here.

use std::fmt;

use bson::Bson;
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{Call, Error, Notification, Outcome};

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
/// It is also what threads a [`Progress::Delegated`](super::Progress::Delegated)
/// exchange together. An execution that pauses for a delegated call is answered
/// once per round trip — the `exec`, then each `resume` — and each of those
/// responses carries the id of the request it is the answer to, so a client waiting
/// on one is never handed the answer to another.
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
/// on how much a command may write — see [`truncated`](super::ExecResult::truncated).
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

    /// `{"jsonrpc":"2.0","id":N,"result":..}` or `{..,"error":..}`
    Response { id: RequestId, outcome: Outcome },
}

/// Which method a request called, and its response answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Start,
    Exec,
    Resume,
    Read,
    Write,
    Stop,
    Quit,
}

impl Method {
    /// The name as it appears in a `method` member.
    pub fn as_str(&self) -> &'static str {
        match self {
            Method::Start => "start",
            Method::Exec => "exec",
            Method::Resume => "resume",
            Method::Read => "read",
            Method::Write => "write",
            Method::Stop => "stop",
            Method::Quit => "quit",
        }
    }

    fn parse(name: &str) -> Option<Method> {
        Some(match name {
            "start" => Method::Start,
            "exec" => Method::Exec,
            "resume" => Method::Resume,
            "read" => Method::Read,
            "write" => Method::Write,
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
// which is also where `jsonrpc` gets checked and `result` xor `error` gets
// enforced, rather than being someone's job later.
//
// What a method's `params` are is not decided here: a `Call` writes and reads its
// own, and so does a `Notification`. This half only knows which of the three shapes
// it is looking at.
impl Serialize for Message {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        // 4 is the largest a JSON-RPC object gets: jsonrpc, id, method, params.
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("jsonrpc", VERSION)?;

        match self {
            Message::Request { id, call } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("method", call.method().as_str())?;
                call.serialize_params(&mut map)?;
            }

            Message::Notification(notification) => {
                map.serialize_entry("method", notification.method().as_str())?;
                notification.serialize_params(&mut map)?;
            }

            Message::Response { id, outcome } => {
                map.serialize_entry("id", id)?;
                match outcome {
                    Outcome::Result(value) => map.serialize_entry("result", value)?,
                    Outcome::Error(error) => map.serialize_entry("error", error)?,
                }
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
        // `params` and `result` are held as values because member order is not
        // guaranteed: `method` may arrive after the `params` it types.
        let mut params: Option<Bson> = None;
        let mut result: Option<Bson> = None;
        let mut error: Option<Error> = None;

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

        if let Some(name) = method {
            if result.is_some() || error.is_some() {
                return Err(de::Error::custom(
                    "a message has a method and a result or error, so it is neither \
                     a request nor a response",
                ));
            }
            let method = Method::parse(&name)
                .ok_or_else(|| de::Error::custom(format!("unknown method {name:?}")))?;

            // An `id` is what says which of the two a method is here as, and each
            // refuses the methods that are not its own. `params` is typed by the
            // method, which is why it waited.
            let params = params.unwrap_or(Bson::Null);

            return match id {
                Some(id) => Ok(Message::Request {
                    id,
                    call: Call::from_params(method, params)?,
                }),
                None => Ok(Message::Notification(Notification::from_params(
                    method, params,
                )?)),
            };
        }

        // No method, so a response: an outcome, and an id to say what it answers.
        // Neither member being there is what makes this none of the three shapes —
        // there was no `method` either, or we would not have got here.
        let outcome = Outcome::from_members(result, error)?
            .ok_or_else(|| de::Error::custom("a message has no method, result or error"))?;
        let id = id.ok_or_else(|| de::Error::custom("a response needs an id"))?;
        Ok(Message::Response { id, outcome })
    }
}

#[cfg(test)]
mod tests {
    use bson::{Document, doc};

    use super::super::{Exec, ExecResult, Progress, Read, ReadResult, Start, Write, WriteResult};
    use super::*;

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

    fn exec() -> Exec {
        Exec {
            // A multi-line `sh -c` script is one argument, and argv's last element
            // can be empty — both are ordinary argv.
            cmd: vec!["sh".into(), "-c".into(), "echo a\necho b".into(), "".into()],
            timeout_ms: Some(1_000),
        }
    }

    /// A whole session on one channel, in order: boot, run something that delegates
    /// twice on the way, release, exit.
    ///
    /// The execution in the middle is the shape worth reading: the `exec` is answered
    /// with a `Delegated` rather than a result, each `resume` is answered with the
    /// next one, and the last of them carries the execution's own ending. Every
    /// request is the client's and every response the server's throughout.
    fn session() -> Vec<Message> {
        vec![
            Message::Request {
                id: 0,
                call: Call::Start(Start {
                    delegated: vec!["bar".into(), "foo".into()],
                    default_timeout_ms: Some(30_000),
                }),
            },
            Message::Response {
                id: 0,
                outcome: Outcome::Result(Bson::Null),
            },
            Message::Request {
                id: 1,
                call: Call::Exec(exec()),
            },
            // Not the execution's result: a name the server cannot run itself, and
            // no timeout of its own.
            Message::Response {
                id: 1,
                outcome: Outcome::Result(
                    bson::serialize_to_bson(&Progress::Delegated(Exec {
                        cmd: vec!["foo".into()],
                        ..Exec::default()
                    }))
                    .unwrap(),
                ),
            },
            // A delegated call that produced something.
            Message::Request {
                id: 2,
                call: Call::Resume(Outcome::Result(
                    bson::serialize_to_bson(&ExecResult {
                        code: 0,
                        stdout: b"foo said this\n".to_vec(),
                        ..ExecResult::default()
                    })
                    .unwrap(),
                )),
            },
            Message::Response {
                id: 2,
                outcome: Outcome::Result(
                    bson::serialize_to_bson(&Progress::Delegated(Exec {
                        cmd: vec!["bar".into(), "--twice".into()],
                        ..Exec::default()
                    }))
                    .unwrap(),
                ),
            },
            // And one that did not, which is why a `resume` carries an outcome and not
            // a result.
            Message::Request {
                id: 3,
                call: Call::Resume(Outcome::Error(Error::new(
                    Error::NOT_EXECUTABLE,
                    "bar: no such executable on the client",
                ))),
            },
            Message::Response {
                id: 3,
                outcome: Outcome::Result(
                    bson::serialize_to_bson(&Progress::Done(ExecResult {
                        code: -1,
                        stdout: vec![0, 1, 2, 255, b'\n'],
                        stderr: vec![],
                        truncated: false,
                    }))
                    .unwrap(),
                ),
            },
            Message::Request {
                id: 4,
                call: Call::Stop,
            },
            Message::Response {
                id: 4,
                outcome: Outcome::Result(Bson::Null),
            },
            Message::Notification(Notification::Quit),
        ]
    }

    /// Everything a session above does not happen to contain.
    fn all() -> Vec<Message> {
        session()
            .into_iter()
            .chain([
                // Delegating nothing, and waiting forever, is a client too.
                Message::Request {
                    id: 6,
                    call: Call::Start(Start::default()),
                },
                Message::Response {
                    id: 6,
                    outcome: Outcome::Error(Error::new(Error::BOOT_FAILED, "no kvm")),
                },
                Message::Response {
                    id: 1,
                    outcome: Outcome::Error(Error::new(Error::TIMED_OUT, "killed after 1000ms")),
                },
                Message::Response {
                    id: 4,
                    outcome: Outcome::Error(Error::new(Error::STOP_FAILED, "guest is wedged")),
                },
                // The file plane. A read bounded on both ends, whose answer is shorter
                // than the file it came from.
                Message::Request {
                    id: 7,
                    call: Call::Read(Read {
                        path: "out/log.txt".into(),
                        offset: Some(4096),
                        len: Some(1024),
                    }),
                },
                Message::Response {
                    id: 7,
                    outcome: Outcome::Result(
                        bson::serialize_to_bson(&ReadResult {
                            data: vec![0xff, 0x00, b'\n'],
                            size: 10_000,
                        })
                        .unwrap(),
                    ),
                },
                Message::Response {
                    id: 7,
                    outcome: Outcome::Error(Error::new(Error::NOT_FOUND, "out/log.txt")),
                },
                // And a whole-file write, which is the offset being absent.
                Message::Request {
                    id: 8,
                    call: Call::Write(Write {
                        path: "in/data".into(),
                        data: vec![0, 1, 2, 255],
                        offset: None,
                    }),
                },
                Message::Response {
                    id: 8,
                    outcome: Outcome::Result(
                        bson::serialize_to_bson(&WriteResult { size: 4 }).unwrap(),
                    ),
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
                call: Call::Exec(Exec {
                    cmd: vec!["ls".into()],
                    ..Exec::default()
                }),
            }),
            doc! {"jsonrpc": "2.0", "id": 2i64, "method": "exec", "params": {"cmd": ["ls"]}},
        );

        assert_eq!(
            wire(&Message::Response {
                id: 2,
                outcome: Outcome::Result(doc! {"code": 0i64}.into()),
            }),
            doc! {"jsonrpc": "2.0", "id": 2i64, "result": {"code": 0i64}},
        );

        assert_eq!(
            wire(&Message::Response {
                id: 2,
                outcome: Outcome::Error(Error::new(Error::TIMED_OUT, "killed after 1000ms")),
            }),
            doc! {
                "jsonrpc": "2.0",
                "id": 2i64,
                "error": {"code": -32000i64, "message": "killed after 1000ms"},
            },
        );

        // A `Delegated` is an ordinary `result`, which is the whole point of it: a peer
        // sees a response and nothing stranger.
        assert_eq!(
            wire(&Message::Response {
                id: 2,
                outcome: Outcome::Result(
                    bson::serialize_to_bson(&Progress::Delegated(Exec {
                        cmd: vec!["foo".into()],
                        ..Exec::default()
                    }))
                    .unwrap()
                ),
            }),
            doc! {
                "jsonrpc": "2.0",
                "id": 2i64,
                "result": {"delegated": {"cmd": ["foo"]}},
            },
        );

        // And a `resume` carries the two members a response spells an outcome with, as
        // its `params`.
        assert_eq!(
            wire(&Message::Request {
                id: 3,
                call: Call::Resume(Outcome::Error(Error::new(Error::TIMED_OUT, "too slow"))),
            }),
            doc! {
                "jsonrpc": "2.0",
                "id": 3i64,
                "method": "resume",
                "params": {"error": {"code": -32000i64, "message": "too slow"}},
            },
        );
        assert_eq!(
            wire(&Message::Request {
                id: 3,
                call: Call::Resume(Outcome::Result(doc! {"code": 0i64}.into())),
            }),
            doc! {
                "jsonrpc": "2.0",
                "id": 3i64,
                "method": "resume",
                "params": {"result": {"code": 0i64}},
            },
        );

        // The file plane spells its bytes as `Binary` like everything else, and the
        // bounds it was given as plain members.
        assert_eq!(
            wire(&Message::Request {
                id: 5,
                call: Call::Read(Read {
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
                call: Call::Write(Write {
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

        // A notification has no id, and a method with no parameters has no
        // `params`: the spec allows omitting it, and `null` is not one of the two
        // types it permits.
        assert_eq!(
            wire(&Message::Notification(Notification::Quit)),
            doc! {"jsonrpc": "2.0", "method": "quit"},
        );
        assert_eq!(
            wire(&Message::Request {
                id: 4,
                call: Call::Stop,
            }),
            doc! {"jsonrpc": "2.0", "id": 4i64, "method": "stop"},
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
            outcome: Outcome::Result(
                bson::serialize_to_bson(&Progress::Done(ExecResult {
                    code: 0,
                    stdout: payload.clone(),
                    ..ExecResult::default()
                }))
                .unwrap(),
            ),
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
        assert_eq!(exec.cmd, ["ls"]);
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

        refused(doc! {"id": 1i64, "method": "stop"}, "jsonrpc");
        refused(
            doc! {"jsonrpc": "1.0", "id": 1i64, "method": "stop"},
            "not \"2.0\"",
        );
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "dance"},
            "unknown method",
        );
        // A request without an id has no way to be answered.
        refused(doc! {"jsonrpc": "2.0", "method": "stop"}, "needs an id");
        // A notification cannot be answered, so it cannot ask to be.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "quit"},
            "cannot carry an id",
        );
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
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "stop", "result": Bson::Null},
            "neither a request nor a response",
        );
        // Params that are not what the method takes.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "exec", "params": {"cmd": "ls"}},
            "exec params",
        );
        // A `resume` carries an outcome, so the same two rules apply to its `params` as
        // to a response's members.
        refused(
            doc! {"jsonrpc": "2.0", "id": 1i64, "method": "resume", "params": {}},
            "no result and no error",
        );
        refused(
            doc! {
                "jsonrpc": "2.0",
                "id": 1i64,
                "method": "resume",
                "params": {"result": Bson::Null, "error": {"code": 1i64, "message": "x"}},
            },
            "both a result and an error",
        );
    }

    /// An unknown member is ignored, so a peer can add one without breaking us.
    #[test]
    fn unknown_members_are_ignored() {
        assert_eq!(
            read(doc! {
                "jsonrpc": "2.0",
                "id": 4i64,
                "method": "stop",
                "trace_id": "abc",
            })
            .unwrap(),
            Message::Request {
                id: 4,
                call: Call::Stop
            },
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
