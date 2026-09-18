//! The methods that are answered, and what each of them asks for.
//!
//! [`Call`] is which one a request names; the rest is what each one carries — an
//! [`InitCall`] and the vocabulary a session is described in, an [`ExecCall`], a
//! [`ReadCall`], a [`WriteCall`], a [`CommitCall`].
//!
//! One file for the asking half and one for the answering half, because that is the
//! division a reader of this protocol has: a client writes calls and reads responses, a
//! server does the reverse, and neither is ever holding both halves of a method at once.
//! It is also the division the envelope names — [`Call`] and [`Response`](super::Response)
//! are the two types a [`Message`](super::Message) is built from — so a file holds exactly
//! what one of those enums can be, and adding a method is one thing to add on each side.
//!
//! The three methods that are answered by nothing are `notification`'s, next door.

use std::path::Path;

use bson::{Bson, doc};
use serde::{Deserialize, Serialize, de};

use super::{CommitCall, Method, utils::bytes};

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
/// {"method":"init","params":{"workfs":{"url":"file:///srv/project"}}}
/// {"method":"exec","params":{"cmd":["sh","-c","ls"],"timeout_ms":1000}}
/// {"method":"read","params":{"path":"out/log.txt","offset":4096,"len":1024}}
/// {"method":"write","params":{"path":"in/data","data":<Binary>}}
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
    /// This is the session: the tree it works in, and what its commands run in and may reach.
    ///
    /// The one exchange that is about the session rather than about work, and the one
    /// thing worth answering about it — because the answer is something a client can
    /// act on before it has asked for anything. A channel that answers this has a
    /// server on the far end that read the frame, speaks this protocol, and has taken
    /// what it was told; a notification could say none of that. The answer also carries
    /// where the workfs went, which is what every later path in the session is spelled
    /// in — see [`InitResp`](super::InitResp).
    ///
    /// It carries no booting, and no mounting either. Bringing a backend up costs a kernel
    /// on one and nothing at all on another, and a session's shape is the same either way,
    /// so *when* to pay for it is [`Start`](super::Notification::Start)'s and not this
    /// method's. The tree is put where this said it would be at the same moment.
    ///
    /// A second one replaces the first, and takes whatever was booted under it with it: the
    /// tree is built into what booting produced, so a session that changes it has a boot that
    /// no longer matches it.
    Init(InitCall),

    /// Run this command.
    Exec(ExecCall),

    /// Hand back part of a file.
    Read(ReadCall),

    /// Put these bytes in a file.
    Write(WriteCall),

    /// Keep what this session has written, as a base a later session can name.
    ///
    /// The last thing a build says. Refused unless the session declared
    /// [`committable`](InitCall::committable) at `init`, because a backend may have had to
    /// arrange for it while booting.
    Commit(CommitCall),
}

impl Call {
    pub fn method(&self) -> Method {
        match self {
            Call::Init(_) => Method::Init,
            Call::Exec(_) => Method::Exec,
            Call::Read(_) => Method::Read,
            Call::Write(_) => Method::Write,
            Call::Commit(_) => Method::Commit,
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

/// What a session is. The `params` of `init`.
///
/// What is here outlives any one execution, which is what it is doing here rather than on
/// an [`ExecCall`]: the tree has to be somewhere before a path can name a file in
/// it, and the base and the reach are the environment a command runs in rather than
/// anything a command says — so each is said once instead of on every command.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitCall {
    /// The tree this session works in, named by URL. `None` is a session with nothing
    /// mounted, which is still a session — a command then sees whatever the executor's own
    /// filesystem holds and nothing this protocol described.
    ///
    /// Answered by a [`WorkFsMount`](super::WorkFsMount) saying where the server put it,
    /// which is what makes every later path in this protocol a path both ends can spell.
    /// See [`InitResp`](super::InitResp).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workfs: Option<WorkFsSource>,

    /// The base a session's commands run in, named as an OCI image. `None` leaves it to the
    /// server.
    ///
    /// **Said once, for the same reason the tree is.** What a command finds on the filesystem
    /// around it is a property of the environment it runs in, not of the command, and on a
    /// backend with a guest it is a disk attached before a kernel comes up.
    ///
    /// Answered in [`InitResp::image`](super::InitResp::image) when what is in force can be
    /// named. A backend whose commands run on the server's own filesystem has no base to
    /// swap and refuses this with
    /// [`UNSUPPORTED_IMAGE`](crate::console::Error::UNSUPPORTED_IMAGE).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSource>,

    /// How much of a network the session's commands get. `None` leaves it to the server.
    ///
    /// **Said once, for the same reason the tree is.** What a command can reach is a property
    /// of the environment it runs in — on some backends a device that has to be attached
    /// before a kernel comes up — so it cannot be decided per `exec` without meaning a
    /// different session for every command.
    ///
    /// Answered in [`InitResp::network`](super::InitResp::network) with what is actually in
    /// force, which is the only way a client learns what it got: `None` here is not "no
    /// network" but "the server's own choice", and a server that runs commands on the host
    /// has no choice to make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkAccess>,

    /// Whether this session may [`commit`](super::CommitCall) what it writes.
    ///
    /// **Said here and not on `commit` because a backend may have to arrange for it while
    /// booting.** A micro-VM one keeps the root that holds its overlay's upper, which is
    /// decided before the first command runs and cannot be decided again after — by the time
    /// a `commit` arrived, the thing it needs would have been gone for the whole session.
    ///
    /// False by default, so a session that will never commit pays nothing for a facility it
    /// does not use, and a `commit` on one is refused rather than answered wrongly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub committable: bool,
}

/// The tree a session works in, named by URL.
///
/// A *workfs* is `fs`'s word for it — a workspace, as against a rootfs — and this is the
/// protocol's way of naming one: not the tree itself, which is a thing in some process, but
/// where to get it.
///
/// # The scheme is the kind
///
/// | scheme | is |
/// |---|---|
/// | `file:///srv/project` | a directory on the server's own filesystem |
/// | `http://…`, `https://…` | a tree reached over HTTP — **on the wire, implemented nowhere** |
///
/// A scheme this build has no provider for is refused at `init` with
/// [`UNSUPPORTED_WORKFS`](crate::console::Error::UNSUPPORTED_WORKFS) naming it, which is
/// what `http` and `https` get everywhere today.
///
/// **A URL and not a tagged object**, because there is exactly one thing this protocol does
/// with it: hand it to whatever realizes that kind. A tagged object would put every kind's
/// settings in this file and make the wire schema grow with the set of providers, where a
/// string leaves the schema alone and leaves each kind's spelling to the kind — a peer that
/// has never heard of a scheme still parses it, and refuses it for the reason it actually
/// has, which is that its *build* has no provider.
///
/// # Why this is an object holding one member
///
/// A kind that has to be *reached* rather than opened needs more than a name for it — an
/// HTTP tree needs whatever authorizes the request, and a secret does not belong in a URL
/// that gets logged, quoted in an error and written into a config file. So the URL is a
/// member rather than the whole of a workfs, and what carries a credential is a member
/// beside it, added when there is a provider that reads one. `file://` needs none, which is
/// why there is none here yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkFsSource {
    /// `file:///srv/project`, `https://example.com/share`.
    pub url: String,
}

impl WorkFsSource {
    pub fn new(url: impl Into<String>) -> Self {
        WorkFsSource { url: url.into() }
    }

    /// The scheme, which is the kind — `"file"`, `"https"`, or the whole URL when it has
    /// no `://` in it and so names no kind at all.
    ///
    /// What a server branches on to decide whether it has a provider, and what it names in
    /// an [`UNSUPPORTED_WORKFS`](crate::console::Error::UNSUPPORTED_WORKFS) when it has not.
    pub fn scheme(&self) -> &str {
        self.url.split_once("://").map_or(&self.url, |(s, _)| s)
    }

    /// The directory a `file://` URL names, or `None` for any other scheme.
    ///
    /// The path is what follows the scheme, **as it stands** — see the type's docs on why
    /// nothing is percent-decoded. Whether it is absolute is the caller's to check and
    /// refuse, because that refusal is a different one: a relative path is a malformed
    /// request where an unknown scheme is a build without a provider.
    ///
    /// Here rather than in each backend because every server that realizes `file://` has to
    /// read it the same way. Two that disagree would be two servers a client cannot tell
    /// apart answering the same URL differently, which is the failure a shared protocol
    /// type exists to prevent.
    pub fn file_path(&self) -> Option<&Path> {
        self.url.strip_prefix("file://").map(Path::new)
    }
}

/// The base a session's commands run in, named as an OCI image.
///
/// # A reference, and nothing around it
///
/// ```
/// # use cortex::console::ImageSource;
/// ImageSource::from("python:3.13-slim");
/// ImageSource::new("ghcr.io/org/tool@sha256:2f0e…");
/// ```
///
/// A reference converts, so a caller with nothing to say beyond the name passes the name:
/// `.image("python:3.13-slim")`.
///
/// The grammar is the registries' own: an optional host, a repository, and a tag or a digest.
/// No scheme is put in front of it, because a reference already says where it comes from and
/// `oci://` in front of a string everybody already types would be friction buying nothing.
///
/// # What is asked for is what is given
///
/// A server provides this base or refuses the session, the same way it treats a
/// [`NetworkAccess`] or a [`WorkFsSource`]. A reference it can parse but not fetch is a
/// different matter: pulling an image is slow enough that it belongs to a boot rather than
/// to `init`, so a name that resolves to nothing is heard from the first call that needs a
/// guest. A client that wants it sooner calls
/// [`Console::start`](crate::console::Console::start).
///
/// # Why an object holding one member
///
/// The same reason [`WorkFsSource`] is one. A platform, a pull policy, a place to put
/// credentials: each is a thing that could be said about an image, and each belongs beside the
/// reference rather than encoded into it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSource {
    /// The OCI reference, as the registries spell one.
    pub reference: String,
}

impl ImageSource {
    pub fn new(reference: impl Into<String>) -> Self {
        ImageSource {
            reference: reference.into(),
        }
    }
}

/// So a caller who has a reference and nothing to say about it writes the reference.
///
/// Nothing is checked here, which is the same as [`new`](ImageSource::new) and for the same
/// reason: whether a string is a reference is the server's to answer, and there is one place
/// that answers it. A convenience that validated would be a second one, disagreeing with the
/// first the day a registry's grammar moves.
impl From<&str> for ImageSource {
    fn from(reference: &str) -> Self {
        ImageSource::new(reference)
    }
}

impl From<String> for ImageSource {
    fn from(reference: String) -> Self {
        ImageSource::new(reference)
    }
}

/// How much of a network a session's commands may reach.
///
/// # The name is the reach
///
/// | `reach` | means |
/// |---|---|
/// | `none` | no network at all |
/// | `host` | enough to resolve a name, and whatever [`host_ports`](Self::host_ports) granted |
/// | `public` | the public internet; not a private range, and not the server's own host |
/// | `full` | whatever the server itself can reach, unrestricted |
///
/// A name this build has no answer for is refused at `init` with
/// [`UNSUPPORTED_NETWORK`](crate::console::Error::UNSUPPORTED_NETWORK) naming it — the same
/// treatment an unknown workfs scheme gets, and for the same reason: a peer that has never
/// heard of a name still parses the frame, and refuses it for the reason it actually has.
///
/// **A name and not an enumeration**, so the wire schema does not grow every time a backend
/// learns a new one. What the set of names *is* belongs to the servers that answer them, and a
/// client that says something none of them knows is told so.
///
/// # What is asked for is what is given
///
/// A server provides the reach named here or refuses the session. It does not narrow one it
/// finds too generous, and it certainly does not widen one — a session that quietly reached
/// more than it asked to is the failure this member exists to prevent, and one that quietly
/// reached less is a client debugging a refused connection it was told it would not get.
///
/// # The reach is how far out, and the ports are which doors in
///
/// [`host_ports`](Self::host_ports) is a second axis and not a fifth name, because **widening
/// what a session reaches outside must not widen what it reaches on the server's own machine.**
/// A session named `public` to fetch a package is not thereby asking to talk to whatever else
/// that machine is listening on, and one granted a port to reach a local model proxy is not
/// asking for the internet.
///
/// So the two are said separately and neither implies the other. It is also what makes `host`
/// worth asking for: on its own it resolves names and no more, and with a port it is how a
/// session talks to something the operator put there on purpose.
///
/// # Why an object and not a string
///
/// The same reason [`WorkFsSource`] is one. A reach is not always a single word — the ports
/// below are the first proof of it, and a list of hosts would be the next — and those belong
/// beside the name rather than encoded into it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAccess {
    /// `none`, `host`, `public`, `full`.
    pub reach: String,

    /// TCP ports on the server's own machine this session may open, on top of whatever
    /// [`reach`](Self::reach) allows. Empty grants none.
    ///
    /// **One port at a time, and never a range meaning "the machine".** A grant is a door onto
    /// something the operator is running there, and the whole reason it is spelled out is that
    /// the convenient way to say "the host" says every port on it.
    ///
    /// Meaningless without a network, so a `none` session naming ports is a contradiction a
    /// server refuses rather than a grant it quietly drops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_ports: Vec<u16>,
}

impl NetworkAccess {
    pub fn new(reach: impl Into<String>) -> Self {
        NetworkAccess {
            reach: reach.into(),
            host_ports: Vec::new(),
        }
    }

    /// The TCP ports on the server's own machine this session may open.
    ///
    /// ```
    /// # use cortex::console::NetworkAccess;
    /// // Resolve names, talk to whatever is on 8080 here, and reach nothing else.
    /// NetworkAccess::host().with_host_ports([8080]);
    /// ```
    ///
    /// # Reaching one from inside
    ///
    /// `host.microsandbox.internal` is the name of the machine the server runs on, answered by
    /// the session's own resolver. A command in the session fetches that name on port 8080 and
    /// gets whatever is listening on 8080 here.
    ///
    /// It has to be a name rather than an address because there is no address to publish: the
    /// backend assigns one per session, so nothing writing a command could know it. Resolving
    /// the name grants nothing on its own, and a port that was not granted is refused whether it
    /// is asked for by name or by number.
    pub fn with_host_ports(mut self, ports: impl IntoIterator<Item = u16>) -> Self {
        self.host_ports = ports.into_iter().collect();
        self
    }

    /// No network at all.
    pub fn none() -> Self {
        NetworkAccess::new("none")
    }

    /// Enough to resolve a name, plus whatever ports were granted.
    pub fn host() -> Self {
        NetworkAccess::new("host")
    }

    /// The public internet.
    pub fn public() -> Self {
        NetworkAccess::new("public")
    }

    /// Whatever the server itself can reach.
    pub fn full() -> Self {
        NetworkAccess::new("full")
    }
}

/// One execution, and everything it needs. The `params` of `exec`.
///
/// A command and a bound on how long it may take, and nothing else — because nothing else
/// about an execution is the client's to say. Where it runs is the session's, which the far
/// end keeps; what it runs with is the executor's; what it reads is whatever a
/// [`WriteCall`] put where it would look.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecCall {
    /// Run this. Already split into argv, and nothing here consults a shell, so quoting
    /// and word rules stay wherever the command was composed; a caller that wants shell
    /// semantics asks for them outright — `["sh", "-c", "..."]`.
    ///
    /// An empty argv is a command nobody can run. It is refused where a command is turned
    /// into a process rather than everywhere an `exec` is read, because that is the end
    /// that knows what running one means — see [`split`](Self::split).
    pub cmd: Vec<String>,

    /// How long this may run before the executor kills it, in milliseconds. `None` is
    /// no limit: an [`InitCall`] carries no default to fall back on, so a command that
    /// never ends and was given no timeout is one nobody ends.
    ///
    /// Expiry is a kill: no grace period, no second signal, no negotiation. One
    /// rule is worth more here than a good one — a requester cannot reach into a
    /// micro-VM guest to check on anything, so anything subtler would be a promise
    /// only some backends could keep.
    ///
    /// The executor enforces it because only the executor knows what running the
    /// thing means. The requester hears [`TIMED_OUT`](crate::console::Error::TIMED_OUT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl ExecCall {
    /// The program and its arguments.
    ///
    /// The split every executor needs, done once here rather than at each of them — and
    /// `None` for an empty argv, which is a command that was never usable and is refused
    /// as [`INVALID_PARAMS`](crate::console::Error::INVALID_PARAMS) by whoever would have spawned
    /// it.
    pub fn split(&self) -> Option<(&String, &[String])> {
        self.cmd.split_first()
    }
}

/// Part of a file to hand back. The `params` of `read`.
///
/// A path is one in the executor's own filesystem, under the
/// [`path`](super::WorkFsMount::path) `init` answered with — so the file this names is the
/// one a command would open by the same name, and reading it is how a requester sees what
/// an execution left behind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadCall {
    /// UTF-8, and the executor's own: a requester builds it by joining onto the path the
    /// session was answered with, which is the one both ends have a name for.
    pub path: String,

    /// Where in the file to start. `None` is the beginning.
    ///
    /// Past the end is not an error: the answer is empty
    /// [`data`](super::ReadResp::data) and the [`size`](super::ReadResp::size) that says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,

    /// How many bytes to hand back at most, `None` for as many as there are.
    ///
    /// Either way the answer travels in one message under
    /// [`MAX_PAYLOAD`](crate::console::MAX_PAYLOAD), so the executor hands back less than this
    /// asks for when the rest would not fit. Comparing what arrives against
    /// [`size`](super::ReadResp::size) is how a requester knows, and asking again from
    /// further along is how it gets the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
}

/// Bytes to put in a file. The `params` of `write`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteCall {
    /// Resolved as [`ReadCall::path`] is. Any directory above it has to
    /// exist already; the file itself does not.
    pub path: String,

    #[serde(default, with = "bytes", skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<u8>,

    /// Where in the file to put them.
    ///
    /// `None` makes the file be exactly `data`: created if it was not there, cut to
    /// length if it was. `Some(n)` overwrites from `n` and leaves whatever lies past
    /// the bytes written, extending the file with zeroes if `n` is beyond its end.
    ///
    /// So the whole-file case says nothing about what was there before and the
    /// positioned case says nothing about the rest of the file, which is why a
    /// requester that means to replace a file sends `None` rather than `Some(0)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

#[cfg(test)]
mod tests {
    use bson::{Document, doc};
    use serde::de::DeserializeOwned;

    use super::*;

    /// Read a value off the bytes a document makes, which is what a peer would actually
    /// have sent.
    fn from_wire<T: DeserializeOwned>(doc: Document) -> Result<T, bson::error::Error> {
        bson::deserialize_from_slice(&bson::serialize_to_vec(&doc).unwrap())
    }

    /// A session with nothing to mount says nothing about mounting: the member is absent
    /// rather than null, which is what leaves room for it to mean exactly one thing when
    /// it is there.
    ///
    /// Which leaves an `init` that describes a session by saying nothing about it, and that
    /// is a session — one whose commands see the executor's own filesystem, with this
    /// protocol having described none of it.
    #[test]
    fn a_session_with_no_workfs_carries_no_workfs() {
        let init = InitCall::default();
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(doc, doc! {});
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc).unwrap(),
            init
        );
    }

    /// The workfs is the URL, which is half of what the two ends have to agree on about
    /// files; where it went is [`InitResp`](super::super::InitResp)'s half.
    #[test]
    fn a_workfs_is_a_url() {
        let init = InitCall {
            workfs: Some(WorkFsSource::new("file:///srv/project")),
            image: None,
            network: None,
            committable: false,
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(doc, doc! {"workfs": {"url": "file:///srv/project"}},);
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc).unwrap(),
            init
        );
    }

    /// A base is a reference, and absent is the shape every session had before there was
    /// one to name.
    #[test]
    fn an_image_is_a_reference() {
        let init = InitCall {
            workfs: None,
            image: Some(ImageSource::new("python:3.13-slim")),
            network: None,
            committable: false,
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(doc, doc! {"image": {"reference": "python:3.13-slim"}});
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc).unwrap(),
            init
        );

        // A reference is enough on its own, and converting is the same as spelling it out.
        assert_eq!(
            ImageSource::from("python:3.13-slim"),
            ImageSource::new("python:3.13-slim"),
        );
        assert_eq!(
            ImageSource::from("python:3.13-slim".to_string()),
            ImageSource::new("python:3.13-slim"),
        );

        // A session that says nothing about a base still serializes to what it always did.
        let quiet = InitCall {
            workfs: None,
            image: None,
            network: None,
            committable: false,
        };
        assert_eq!(bson::serialize_to_document(&quiet).unwrap(), doc! {},);
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc! {}).unwrap(),
            quiet,
        );
    }

    /// A reach is a name, and absent is the shape every session had before there was one
    /// to name.
    #[test]
    fn a_network_is_a_name() {
        let init = InitCall {
            workfs: None,
            image: None,
            network: Some(NetworkAccess::public()),
            committable: false,
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(doc, doc! {"network": {"reach": "public"}});
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc).unwrap(),
            init
        );

        // **A session that says nothing about a network still serializes to what it always
        // did**, which is what lets a client of this build talk to a server that predates the
        // member — and a server of this build answer a client that does.
        let quiet = InitCall {
            workfs: None,
            image: None,
            network: None,
            committable: false,
        };
        assert_eq!(bson::serialize_to_document(&quiet).unwrap(), doc! {},);
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc! {}).unwrap(),
            quiet,
        );
    }

    /// A grant travels beside the name, and a session without one serializes to what it always
    /// did — which is what lets this member arrive without every existing peer noticing.
    #[test]
    fn a_host_port_grant_is_said_beside_the_reach() {
        let init = InitCall {
            workfs: None,
            image: None,
            network: Some(NetworkAccess::host().with_host_ports([8080, 3000])),
            committable: false,
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(
            doc,
            doc! {"network": {"reach": "host", "host_ports": [8080, 3000]}}
        );
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc).unwrap(),
            init
        );

        // No grant is no member, not an empty one.
        let doc = bson::serialize_to_document(&NetworkAccess::host()).unwrap();
        assert_eq!(doc, doc! {"reach": "host"});
        assert_eq!(
            bson::deserialize_from_document::<NetworkAccess>(doc).unwrap(),
            NetworkAccess::host(),
        );

        // And the two axes are independent: naming one says nothing about the other.
        assert!(NetworkAccess::public().host_ports.is_empty());
        assert_eq!(
            NetworkAccess::host().with_host_ports([8080]).reach,
            NetworkAccess::host().reach,
        );
    }

    /// The names are constructors so a caller does not spell them, and they are what a server
    /// branches on — so the two have to be the same strings.
    #[test]
    fn the_names_are_the_ones_a_server_reads() {
        assert_eq!(NetworkAccess::none().reach, "none");
        assert_eq!(NetworkAccess::host().reach, "host");
        assert_eq!(NetworkAccess::public().reach, "public");
        assert_eq!(NetworkAccess::full().reach, "full");
    }

    /// The two things a server reads off a URL, and the one it can act on.
    #[test]
    fn a_url_names_a_kind_and_sometimes_a_directory() {
        let file = WorkFsSource::new("file:///srv/project");
        assert_eq!(file.scheme(), "file");
        assert_eq!(file.file_path(), Some(Path::new("/srv/project")));

        let http = WorkFsSource::new("https://example.com/share");
        assert_eq!(http.scheme(), "https");
        assert_eq!(http.file_path(), None);

        // No scheme at all names no kind, and the whole of it is what a server has to put
        // in the message — there is nothing shorter that would say what arrived.
        let bare = WorkFsSource::new("/srv/project");
        assert_eq!(bare.scheme(), "/srv/project");
        assert_eq!(bare.file_path(), None);

        // An authority is not decoded away: what follows the scheme is the path, so this
        // one is relative and the caller is the end that refuses it.
        assert_eq!(
            WorkFsSource::new("file://srv/project").file_path(),
            Some(Path::new("srv/project"))
        );
    }

    /// An execution that sets no timeout is a `cmd` and nothing else: the member is absent
    /// rather than null, which is what `skip_serializing_if` buys — and it reads back
    /// unset.
    #[test]
    fn an_unadorned_exec_is_a_command_and_nothing_else() {
        let exec = ExecCall {
            cmd: vec!["ls".into()],
            ..ExecCall::default()
        };
        let doc = bson::serialize_to_document(&exec).unwrap();
        assert_eq!(doc, doc! {"cmd": ["ls"]});
        assert_eq!(from_wire::<ExecCall>(doc).unwrap(), exec);

        let bounded = ExecCall {
            timeout_ms: Some(5_000),
            ..exec
        };
        let doc = bson::serialize_to_document(&bounded).unwrap();
        assert_eq!(doc, doc! {"cmd": ["ls"], "timeout_ms": 5_000i64});
        assert_eq!(from_wire::<ExecCall>(doc).unwrap(), bounded);
    }

    /// An unknown member is ignored here as it is everywhere else in this protocol, so a
    /// peer may add one.
    #[test]
    fn an_unknown_member_is_ignored() {
        assert_eq!(
            from_wire::<ExecCall>(doc! {"cmd": ["ls"], "took_ms": 3i64})
                .unwrap()
                .cmd,
            vec!["ls".to_string()],
        );
    }

    /// An empty command is refused where a command becomes a process, which is why it is
    /// a `None` here rather than a rejection at the frame.
    #[test]
    fn a_command_splits_into_a_program_and_its_arguments() {
        let exec = ExecCall {
            cmd: vec!["sh".into(), "-c".into(), "".into()],
            ..ExecCall::default()
        };
        let (program, args) = exec.split().unwrap();
        assert_eq!(program, "sh");
        assert_eq!(args, ["-c", ""]);

        assert!(ExecCall::default().split().is_none());
    }

    /// A read with no bounds carries neither member, so the whole-file case is the
    /// smallest thing the method can say.
    #[test]
    fn an_unbounded_read_carries_no_bounds() {
        let read = ReadCall {
            path: "log.txt".into(),
            ..ReadCall::default()
        };
        let doc = bson::serialize_to_document(&read).unwrap();
        assert_eq!(doc, doc! {"path": "log.txt"});
        assert_eq!(
            bson::deserialize_from_document::<ReadCall>(doc).unwrap(),
            read
        );
    }

    /// A write with no offset carries no offset member: replacing a file is the
    /// smallest thing the method can say.
    #[test]
    fn a_whole_file_write_carries_no_offset() {
        let write = WriteCall {
            path: "data".into(),
            data: vec![1, 2, 3],
            offset: None,
        };
        let doc = bson::serialize_to_document(&write).unwrap();
        assert_eq!(doc.get("offset"), None);
        assert_eq!(
            bson::deserialize_from_document::<WriteCall>(doc).unwrap(),
            write,
        );
    }

    /// False says nothing, so a session that will never commit costs nothing to describe —
    /// and a server that predates the member reads one correctly.
    #[test]
    fn a_session_that_will_not_commit_says_nothing() {
        let init = InitCall::default();
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(doc.get("committable"), None);
        assert!(
            !bson::deserialize_from_document::<InitCall>(doc)
                .unwrap()
                .committable
        );
    }

    #[test]
    fn a_session_that_might_commit_says_so() {
        let init = InitCall {
            committable: true,
            ..InitCall::default()
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(doc.get("committable"), Some(&bson::Bson::Boolean(true)));
        assert!(
            bson::deserialize_from_document::<InitCall>(doc)
                .unwrap()
                .committable
        );
    }
}
