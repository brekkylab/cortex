//! The answering half: what a request was answered with.
//!
//! [`Response`] is the enum over the four answers, the [`Error`] that stands where a
//! result would have been, and the serde impls that put either of them on the wire; the
//! rest is what each method answers with — an [`InitResp`], an [`ExecResp`], a
//! [`ReadResp`], a [`WriteResp`].
//!
//! One file for the answering half and one for the asking half, for the reason `call`
//! gives: it is the division a reader of this protocol has, and the one the envelope
//! names.
//!
//! What a call asks *for* is spelled in `call`'s vocabulary, and an answer says what came
//! of it in the same words — an [`ImageSource`](super::ImageSource) is what was asked and
//! what is in force. So those types are declared beside the call that first says them and
//! echoed here, rather than copied into a second spelling that could disagree.

use std::fmt;

use bson::{Bson, Document};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, DeserializeOwned, MapAccess, Visitor},
    ser::SerializeMap,
};

use super::{CommitResp, Error, ImageSource, Method, NetworkAccess, utils::bytes};

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
/// {"method":"init","result":{"workfs":{"mount":"/work"},"cwd":"/work"}}
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
    Init(InitResp),
    Exec(ExecResp),
    Read(ReadResp),
    Write(WriteResp),
    Commit(CommitResp),

    /// Why the method produced no result. See [`Error`] for the codes.
    Error(Error),
}

impl Response {
    /// Which method this answers, or `None` for an [`Error`](Response::Error) — which
    /// answers whichever one asked, and says so with a code rather than a name.
    pub fn method(&self) -> Option<Method> {
        Some(match self {
            Response::Commit(_) => Method::Commit,
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
            Response::Init(init) => map.serialize_entry("result", init)?,
            Response::Exec(exec) => map.serialize_entry("result", exec)?,
            Response::Read(read) => map.serialize_entry("result", read)?,
            Response::Write(write) => map.serialize_entry("result", write)?,
            Response::Commit(commit) => map.serialize_entry("result", commit)?,
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
        Method::Commit => Response::Commit(payload(method, result)?),
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

/// What the server made of the session. The `result` of `init`.
///
/// Answered rather than left to a notification because this is the one thing about a
/// session a client can hear before it asks for work — that there is a server on the far
/// end, that it read the frame, that it speaks this protocol, and that it has taken what it
/// was told. It is also *where*: a session with a workfs has a path in it, and that path is
/// what every later `read` and `write` is spelled in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitResp {
    /// Where the workfs [`InitCall::workfs`](super::InitCall::workfs) named went, or `None`
    /// when none was named.
    ///
    /// A client that asked for one and is answered without this has been told nothing it
    /// can use: every path it would send afterwards would be a guess. That is a broken
    /// session rather than an empty one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workfs: Option<WorkFsMount>,

    /// Where the session stands to begin with — conventionally the
    /// [`workfs`](Self::workfs) mount point, and never required to be.
    ///
    /// **A session has a current directory, and the server is what keeps it.** That is why
    /// an [`ExecCall`](super::ExecCall) asking for a command says nothing about where to run it:
    /// there is one answer at any moment and the far end holds it.
    ///
    /// **To begin with**, and nothing here says otherwise afterwards. A command can move
    /// the session — `cd` is a shell builtin, so a backend that offers it at all answers it
    /// itself — and no result reports that it did. A client that wants to know where it
    /// stands runs `pwd`, the way a person at a terminal does; see [`ExecResp`] for why
    /// that is the trade rather than a gap.
    ///
    /// So what this is worth is the *first* answer: before a client has run anything, this
    /// is the only way it can say where a relative path would land. Absent is a server that
    /// will not say, and a client is then no worse off than it was before the field existed
    /// — every path it sends is one it built itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    /// The base in force, when it is one that can be named.
    ///
    /// The [`InitCall::image`](super::InitCall::image) that was asked for, in the server's own
    /// spelling of it: a reference with no registry named is a reference to a default
    /// registry, and this is where a client learns which one that was. What makes it worth
    /// answering is the case where nothing was asked, since the base is then the server's
    /// own choice.
    ///
    /// Absent is a base with no reference to give: a server that will not say, or one running
    /// on something that is not an OCI image at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSource>,

    /// What the session's commands can actually reach.
    ///
    /// The [`InitCall::network`](super::InitCall::network) that was asked for, when one was — a
    /// server provides that reach or refuses, so this confirms rather than negotiates. What
    /// makes it worth answering is the case where nothing was asked: the reach is then the
    /// server's own, and this is the only place a client can learn which.
    ///
    /// Absent is a server that will not say, and a client that asked for nothing then knows
    /// nothing — which is where every client was before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkAccess>,
}

/// Where a workfs is, in the server's filesystem.
///
/// # Why the server says the path instead of both ends agreeing on a namespace
///
/// A `read` and a command have to name one file, which needs one name that means one file to
/// both ends. Two ends realizing the same description separately is one way to get that, and
/// it costs a tree on each side, a mount on each side, and a rewriting step on every path
/// that crosses — all to reconstruct something one end already has.
///
/// So the server realizes it once and says where. Everything after this is that path: a
/// `read` names a file under it, an execution's reported directory is a directory under it,
/// and neither end rewrites anything. What it costs is that the client has to be able to
/// open what the server opened — the two share a filesystem, which is what the path being
/// *the server's* means. A backend whose commands run somewhere else, a guest included,
/// answers a path on this side of that boundary and either translates behind it or arranges
/// that there is nothing to translate: `cortex-uvm-console` shares the host's directory into
/// its guest **at the host's own path**, so the two spellings are one string and a `cwd` the
/// guest reports needs no rewriting to be a name the client can open.
///
/// # It is a name before it is a directory
///
/// `init` mounts nothing, the way it boots nothing: the mount happens when the session
/// boots, at the path already answered here. So this is fixed for the session, and a kernel
/// answers at it only while something is booted. Nothing needs it any sooner — `read`,
/// `write` and `exec` each boot a session first — which is what makes a path answered
/// before there is a directory at it useful rather than a promise.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkFsMount {
    /// Absolute, and in the server's filesystem — `"/mnt/workfs"`.
    ///
    /// A relative one would be relative to a working directory nobody named, and the client
    /// has no way to guess which.
    pub path: String,
}

/// A whole execution in one value: everything it wrote, and how it ended. The `result` of
/// `exec`.
///
/// A failure is not one of these. An execution that produced no result at all — killed at
/// its timeout, or never started — travels as an `error` with a code on it, see
/// [`Error`]: a code an ordinary `exit()` can reach could never have proved the difference.
///
/// It goes on the wire unwrapped, so what a peer adds to this later is a *member*. That is
/// the extension this protocol already handles — an unknown member is ignored everywhere —
/// where a tagged alternative beside it would be a shape an older peer could only fail on.
///
/// # What is not here: where the session ended up
///
/// A session has a current directory and an execution can move it — see
/// [`InitResp::cwd`] — and none of that is reported back on this.
///
/// Because a shell does not report it either. A terminal answers `cd work` with nothing at
/// all, and a person who wants to know where they are types `pwd`; a client is in exactly
/// that position, and `pwd` is an `exec` like any other. A member repeated on every result
/// to say "unmoved" for nearly every one of them is a member a reader stops looking at, and
/// the one time it matters is the one time it can be asked for.
///
/// What that costs is a round trip, on the executions where a client actually needs the
/// answer. What it buys is that a result describes the *command* — what it wrote, how it
/// ended — and nothing about the machine it ran on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResp {
    /// A command killed by a signal has no code of its own; the convention is
    /// `128 + signal`.
    pub code: i32,

    /// Kept apart from [`stderr`](Self::stderr) because merging is something a
    /// requester can do and un-merging is not: a value the caller wanted only the
    /// data from, and a diagnostic it wanted to show or log, are two different
    /// things once they have arrived. The interleaving between them is not
    /// preserved — two buffers are not one stream — and a caller that needs the
    /// order asks the command for it (`2>&1`).
    #[serde(with = "bytes")]
    pub stdout: Vec<u8>,

    #[serde(with = "bytes")]
    pub stderr: Vec<u8>,

    /// Whether the command wrote more than the executor was willing to hold, and
    /// what is here is the beginning of it.
    ///
    /// A result travels in one message under [`MAX_PAYLOAD`](crate::console::MAX_PAYLOAD), so
    /// a command that writes without limit has to be cut off somewhere. Saying so is
    /// the whole point of the field: an agent reading output it does not know is
    /// partial will draw a conclusion from it, and a wrong answer is worse than a
    /// short one.
    #[serde(default)]
    pub truncated: bool,
}

/// The bytes a `read` asked for, and how big the file is. The `result` of `read`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResp {
    /// What was there, starting at the [`offset`](super::ReadCall::offset) that was asked
    /// for.
    #[serde(with = "bytes")]
    pub data: Vec<u8>,

    /// The whole file's size, and not `data`'s length.
    ///
    /// The two differ whenever a read was bounded — by a [`len`](super::ReadCall::len), by
    /// an [`offset`](super::ReadCall::offset) past the beginning, or by what one message
    /// holds — and the difference is the only thing that says there is more to ask for. A
    /// reader that ignores it has no way to tell a whole small file from the front of a
    /// large one.
    pub size: u64,
}

/// How big the file is now. The `result` of `write`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteResp {
    /// Where a positioned write should carry on from, and confirmation that a
    /// whole-file write left the length it meant to.
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::{
        super::{InitCall, WorkFsSource},
        *,
    };

    /// A session with nothing mounted is answered with nothing about mounting: the member
    /// is absent rather than null, which is what leaves room for it to mean exactly one
    /// thing when it is there.
    #[test]
    fn an_answer_with_no_workfs_carries_no_workfs() {
        let answered = InitResp::default();
        let doc = bson::serialize_to_document(&answered).unwrap();
        assert_eq!(doc, doc! {});
        assert_eq!(
            bson::deserialize_from_document::<InitResp>(doc).unwrap(),
            answered
        );
    }

    /// The answer to a workfs is where the tree went and where the session stands in it —
    /// the second conventionally the first, which is what the two members being apart
    /// allows to not be the case.
    #[test]
    fn the_answer_to_a_workfs_is_where_it_went() {
        let answered = InitResp {
            workfs: Some(WorkFsMount {
                path: "/mnt/workfs".into(),
            }),
            cwd: Some("/mnt/workfs/work".into()),
            image: None,
            network: None,
        };
        let doc = bson::serialize_to_document(&answered).unwrap();
        assert_eq!(
            doc,
            doc! {"workfs": {"path": "/mnt/workfs"}, "cwd": "/mnt/workfs/work"},
        );
        assert_eq!(
            bson::deserialize_from_document::<InitResp>(doc).unwrap(),
            answered,
        );

        // Unknown members are ignored here as everywhere else, so a server may report more
        // about what it mounted than this reads — and a server that will not say where the
        // session stands is answered without that member rather than with a guess.
        assert_eq!(
            bson::deserialize_from_document::<InitResp>(
                doc! {"workfs": {"path": "/mnt/workfs", "kind": "local"}}
            )
            .unwrap(),
            InitResp {
                workfs: Some(WorkFsMount {
                    path: "/mnt/workfs".into(),
                }),
                cwd: None,
                image: None,
                network: None,
            },
        );

        // And what was asked for is the URL, which is the other half of the one name the
        // two ends have to agree on.
        assert_eq!(
            InitCall {
                workfs: Some(WorkFsSource::new("file:///srv/project")),
                ..InitCall::default()
            }
            .workfs
            .unwrap()
            .url,
            "file:///srv/project",
        );
    }

    /// **The answer is the server's spelling and not an echo.** A reference naming no
    /// registry names a default one, and which default that was is the thing a client could
    /// not have worked out for itself.
    #[test]
    fn the_answered_image_is_the_one_in_force() {
        let answered = InitResp {
            workfs: None,
            cwd: None,
            image: Some(ImageSource::new("docker.io/library/python:3.13-slim")),
            network: None,
        };
        let doc = bson::serialize_to_document(&answered).unwrap();
        assert_eq!(
            doc,
            doc! {"image": {"reference": "docker.io/library/python:3.13-slim"}}
        );
        assert_eq!(
            bson::deserialize_from_document::<InitResp>(doc).unwrap(),
            answered,
        );
    }

    /// A reach is a name in both directions, and absent on either side is the shape every
    /// session had before there was one to name.
    #[test]
    fn the_answered_network_is_the_one_in_force() {
        let answered = InitResp {
            workfs: None,
            cwd: None,
            image: None,
            network: Some(NetworkAccess::host()),
        };
        let doc = bson::serialize_to_document(&answered).unwrap();
        assert_eq!(doc, doc! {"network": {"reach": "host"}});
        assert_eq!(
            bson::deserialize_from_document::<InitResp>(doc).unwrap(),
            answered,
        );

        // A server that says nothing about a network is read as saying nothing, which is
        // what lets a client of this build talk to a server that predates the member.
        assert_eq!(
            bson::deserialize_from_document::<InitResp>(doc! {}).unwrap(),
            InitResp::default(),
        );
    }

    /// Not utf-8, contains NUL and a newline: nothing about program bytes is special,
    /// and BSON carries them as themselves rather than as text.
    ///
    /// The assertion is on the *variant* and not only the round trip, because a round
    /// trip passes either way — base64 out and base64 back is symmetric, and would be
    /// 1.37× and the byte type unused with nothing to show it. That is the failure this
    /// test exists to catch; see [`bytes`](super::super::utils::bytes) on why the codec is not
    /// asked.
    #[test]
    fn program_bytes_survive_as_bytes() {
        let bytes = vec![0xff, 0xfe, 0x00, b'\n', 0x00];
        let value = bson::serialize_to_bson(&ExecResp {
            stdout: bytes.clone(),
            stderr: bytes.clone(),
            ..ExecResp::default()
        })
        .unwrap();

        let doc = value.as_document().unwrap();
        assert_eq!(
            doc.get("stdout"),
            Some(&Bson::Binary(bson::Binary {
                subtype: bson::spec::BinarySubtype::Generic,
                bytes: bytes.clone(),
            })),
        );

        let read: ExecResp = bson::deserialize_from_bson(value).unwrap();
        assert_eq!((read.stdout, read.stderr), (bytes.clone(), bytes));
    }

    /// The `result` of an `exec` is the ending itself, with nothing around it — so what a
    /// peer adds to it later is a member, which every reader here already ignores when it
    /// does not know it.
    #[test]
    fn an_ending_is_the_result_with_nothing_around_it() {
        let ended = ExecResp {
            code: 0,
            stdout: b"hi\n".to_vec(),
            ..ExecResp::default()
        };
        let doc = bson::serialize_to_document(&ended).unwrap();
        assert!(doc.contains_key("code"), "{doc:?}");
        assert_eq!(
            bson::deserialize_from_document::<ExecResp>(doc).unwrap(),
            ended
        );

        // A member nobody here has heard of reads back as the ending it always was.
        let mut future = bson::serialize_to_document(&ended).unwrap();
        future.insert("suspended_at", 3i64);
        assert_eq!(
            bson::deserialize_from_document::<ExecResp>(future).unwrap(),
            ended
        );
    }

    /// A read that hands back less than the file holds is the ordinary case, and
    /// `size` against `data` is the only thing that says so.
    #[test]
    fn a_read_result_says_how_much_it_left() {
        let value = bson::serialize_to_bson(&ReadResp {
            data: vec![0xff, 0x00],
            size: 4096,
        })
        .unwrap();

        let doc = value.as_document().unwrap();
        assert_eq!(
            doc.get("data"),
            Some(&Bson::Binary(bson::Binary {
                subtype: bson::spec::BinarySubtype::Generic,
                bytes: vec![0xff, 0x00],
            })),
        );

        let read: ReadResp = bson::deserialize_from_bson(value).unwrap();
        assert!(read.size > read.data.len() as u64);
    }
}
