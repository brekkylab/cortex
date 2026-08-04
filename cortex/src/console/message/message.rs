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

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

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
            Method::Stop => "stop",
            Method::Quit => "quit",
        }
    }

    fn parse(name: &str) -> Option<Method> {
        Some(match name {
            "start" => Method::Start,
            "exec" => Method::Exec,
            "resume" => Method::Resume,
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
        let mut params: Option<Value> = None;
        let mut result: Option<Value> = None;
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
            let params = params.unwrap_or(Value::Null);

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
    use serde_json::json;

    use super::super::{Exec, ExecResult, Progress, Start};
    use super::*;

    fn exec() -> Exec {
        Exec {
            // A multi-line `sh -c` script is one argument, and argv's last element
            // can be empty — both are ordinary argv.
            cmd: vec!["sh".into(), "-c".into(), "echo a\necho b".into(), "".into()],
            stdin: vec![0xff, 0x00, b'\n'],
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
                outcome: Outcome::Result(Value::Null),
            },
            Message::Request {
                id: 1,
                call: Call::Exec(exec()),
            },
            // Not the execution's result: a name the server cannot run itself. No
            // input, no timeout of its own.
            Message::Response {
                id: 1,
                outcome: Outcome::Result(
                    serde_json::to_value(Progress::Delegated(Exec {
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
                    serde_json::to_value(ExecResult {
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
                    serde_json::to_value(Progress::Delegated(Exec {
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
                    serde_json::to_value(Progress::Done(ExecResult {
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
                outcome: Outcome::Result(Value::Null),
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
            ])
            .collect()
    }

    #[test]
    fn messages_survive_a_json_roundtrip() {
        for message in all() {
            let text = serde_json::to_string(&message).unwrap();
            assert_eq!(
                serde_json::from_str::<Message>(&text).unwrap(),
                message,
                "{text}"
            );
        }
    }

    /// The whole reason for the manual impls: these are the bytes the spec calls
    /// for, member for member.
    #[test]
    fn the_wire_is_json_rpc_2_0() {
        let wire = |message: &Message| serde_json::to_value(message).unwrap();

        assert_eq!(
            wire(&Message::Request {
                id: 2,
                call: Call::Exec(Exec {
                    cmd: vec!["ls".into()],
                    ..Exec::default()
                }),
            }),
            json!({"jsonrpc": "2.0", "id": 2, "method": "exec", "params": {"cmd": ["ls"]}}),
        );

        assert_eq!(
            wire(&Message::Response {
                id: 2,
                outcome: Outcome::Result(json!({"code": 0})),
            }),
            json!({"jsonrpc": "2.0", "id": 2, "result": {"code": 0}}),
        );

        assert_eq!(
            wire(&Message::Response {
                id: 2,
                outcome: Outcome::Error(Error::new(Error::TIMED_OUT, "killed after 1000ms")),
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "error": {"code": -32000, "message": "killed after 1000ms"},
            }),
        );

        // A `Delegated` is an ordinary `result`, which is the whole point of it: an
        // off-the-shelf peer sees a response and nothing stranger.
        assert_eq!(
            wire(&Message::Response {
                id: 2,
                outcome: Outcome::Result(
                    serde_json::to_value(super::super::Progress::Delegated(Exec {
                        cmd: vec!["foo".into()],
                        ..Exec::default()
                    }))
                    .unwrap()
                ),
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"delegated": {"cmd": ["foo"]}},
            }),
        );

        // And a `resume` carries the two members a response spells an outcome with, as
        // its `params`.
        assert_eq!(
            wire(&Message::Request {
                id: 3,
                call: Call::Resume(Outcome::Error(Error::new(Error::TIMED_OUT, "too slow"))),
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "resume",
                "params": {"error": {"code": -32000, "message": "too slow"}},
            }),
        );
        assert_eq!(
            wire(&Message::Request {
                id: 3,
                call: Call::Resume(Outcome::Result(json!({"code": 0}))),
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "resume",
                "params": {"result": {"code": 0}},
            }),
        );

        // A notification has no id, and a method with no parameters has no
        // `params`: the spec allows omitting it, and `null` is not one of the two
        // types it permits.
        assert_eq!(
            wire(&Message::Notification(Notification::Quit)),
            json!({"jsonrpc": "2.0", "method": "quit"}),
        );
        assert_eq!(
            wire(&Message::Request {
                id: 4,
                call: Call::Stop,
            }),
            json!({"jsonrpc": "2.0", "id": 4, "method": "stop"}),
        );
    }

    /// Member order is the sender's business, not ours — `params` may arrive
    /// before the `method` that types it.
    #[test]
    fn members_may_arrive_in_any_order() {
        let text = r#"{"params":{"cmd":["ls"]},"id":2,"method":"exec","jsonrpc":"2.0"}"#;
        let Message::Request {
            id: 2,
            call: Call::Exec(exec),
        } = serde_json::from_str(text).unwrap()
        else {
            panic!("wrong message")
        };
        assert_eq!(exec.cmd, ["ls"]);
    }

    /// Nothing a peer could send that is not one of the three shapes gets through
    /// as one of them.
    #[test]
    fn malformed_objects_are_refused() {
        let refused = |text: &str, because: &str| {
            let error = serde_json::from_str::<Message>(text)
                .expect_err(&format!("accepted {text}"))
                .to_string();
            assert!(error.contains(because), "{text} → {error}");
        };

        refused(r#"{"id":1,"method":"stop"}"#, "jsonrpc");
        refused(r#"{"jsonrpc":"1.0","id":1,"method":"stop"}"#, "not \"2.0\"");
        refused(
            r#"{"jsonrpc":"2.0","id":1,"method":"dance"}"#,
            "unknown method",
        );
        // A request without an id has no way to be answered.
        refused(r#"{"jsonrpc":"2.0","method":"stop"}"#, "needs an id");
        // A notification cannot be answered, so it cannot ask to be.
        refused(
            r#"{"jsonrpc":"2.0","id":1,"method":"quit"}"#,
            "cannot carry an id",
        );
        // `result` xor `error`, and one of them.
        refused(
            r#"{"jsonrpc":"2.0","id":1,"result":null,"error":{"code":1,"message":"x"}}"#,
            "both a result and an error",
        );
        refused(r#"{"jsonrpc":"2.0","id":1}"#, "no method, result or error");
        refused(
            r#"{"jsonrpc":"2.0","id":1,"method":"stop","result":null}"#,
            "neither a request nor a response",
        );
        // Params that are not what the method takes.
        refused(
            r#"{"jsonrpc":"2.0","id":1,"method":"exec","params":{"cmd":"ls"}}"#,
            "exec params",
        );
        // A `resume` carries an outcome, so the same two rules apply to its `params` as
        // to a response's members.
        refused(
            r#"{"jsonrpc":"2.0","id":1,"method":"resume","params":{}}"#,
            "no result and no error",
        );
        refused(
            r#"{"jsonrpc":"2.0","id":1,"method":"resume","params":{"result":null,"error":{"code":1,"message":"x"}}}"#,
            "both a result and an error",
        );
    }

    /// An unknown member is ignored, so a peer can add one without breaking us.
    #[test]
    fn unknown_members_are_ignored() {
        let text = r#"{"jsonrpc":"2.0","id":4,"method":"stop","trace_id":"abc"}"#;
        assert_eq!(
            serde_json::from_str::<Message>(text).unwrap(),
            Message::Request {
                id: 4,
                call: Call::Stop
            },
        );
    }

    /// A response's `result` is typed by the method its id was issued for, which
    /// only the end that issued it knows.
    #[test]
    fn a_result_is_typed_by_the_method_the_caller_remembers() {
        let result = Outcome::Result(
            serde_json::to_value(ExecResult {
                code: 3,
                stdout: b"out".to_vec(),
                ..ExecResult::default()
            })
            .unwrap(),
        );
        assert_eq!(result.clone().take::<ExecResult>().unwrap().code, 3);
        // `start` and `stop` return nothing, and nothing is what `null` is.
        Outcome::Result(Value::Null).take::<()>().unwrap();

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
