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

use std::path::Path;

use bson::{Bson, doc};
use serde::{Deserialize, Serialize, de};

use crate::rootfs_v2::RootFsV2;

use super::{Method, utils::bytes};

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
/// {"method":"init","params":{"context":{"url":"file:///srv/project"}}}
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
    /// This is the session: the trees it works in, and what its commands run in and may reach.
    ///
    /// The one exchange that is about the session rather than about work, and the one
    /// thing worth answering about it — because the answer is something a client can
    /// act on before it has asked for anything. A channel that answers this has a
    /// server on the far end that read the frame, speaks this protocol, and has taken
    /// what it was told; a notification could say none of that. The answer also carries
    /// where each tree went, which is what every later path in the session is spelled
    /// in — see [`InitResp`](super::InitResp).
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

/// What a session is. The `params` of `init`.
///
/// What is here outlives any one execution, which is what it is doing here rather than on
/// an [`ExecCall`]: a tree has to be somewhere before a path can name a file in
/// it, and the base and the reach are the environment a command runs in rather than
/// anything a command says — so each is said once instead of on every command.
///
/// # Two trees, because they are two lifetimes
///
/// [`context`](Self::context) and [`artifacts`](Self::artifacts) are each a [`TreeSource`]
/// and each answered with a [`TreeMount`](super::TreeMount), and what tells them apart is
/// the member name rather than anything in the value.
///
/// | member | is | outlives the session |
/// |---|---|---|
/// | `context` | what the session is given to work **from**, and reads | yes — it was there before |
/// | `artifacts` | what the session is to leave behind | yes — that is the point of it |
///
/// **Which is why they are two members and not one tree with two directories in it.**
/// A single tree makes the two the same thing to everyone holding it: the same store
/// behind them, the same lifetime, the same permissions, and a client that wants to keep
/// what a session produced has to know which subdirectory that was and trust the session
/// not to have written outside it. Naming them separately is what lets each be backed by
/// what it should be — a project directory, a bucket the caller collects from — and it is
/// the protocol's only way to say which of them a path is in.
///
/// Room to work in is not a third of them. A session already stands on a filesystem it may
/// write to and that goes away with it, so a command that unpacks an archive or builds
/// something has somewhere to put it without the client naming a tree for it — and a tree
/// named for that purpose would be one more thing to mount, place and answer for, in
/// exchange for what the session's own root already gives.
///
/// It is a departure from [`ContextFs`](crate::fs::ContextFs)'s composition, which is how a
/// session gets *many stores* in one tree, and the two answer different questions. Several
/// stores under one root are one namespace a command walks; these are separate namespaces a
/// client has separate intentions for.
///
/// Both are independently optional, and a session with neither is still a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitCall {
    /// The base a session's commands run in.
    ///
    /// It is essential if it runs on VM environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rootfs: Option<RootFsV2>,

    /// Optional snapshot of what an earlier session changed from the base rootfs — what a
    /// [`snapshot`](SnapshotCall) answered with, handed back.
    ///
    /// Useful when a session does not start from scratch: it starts with those changes already
    /// in place, as though it were the same session carrying on.
    ///
    /// **A layer tar**: the files that session wrote, with the ones it deleted carried as OCI
    /// whiteouts. Not an image of the filesystem they lived on, and the difference is what
    /// makes this a thing a frame can hold — a filesystem image brings its own metadata, its
    /// journal and all the room it was formatted to, none of which is the session's work. It
    /// also costs an executor nothing to apply: a layer is what one already knows how to put
    /// in front of a base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Vec<u8>>,

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

    /// The tree this session works **from**, named by URL. `None` is a session with nothing
    /// mounted, which is still a session — a command then sees whatever the executor's own
    /// filesystem holds and nothing this protocol described.
    ///
    /// **What the session is given, as against what it makes.** It was there before the
    /// session and outlives it, which is why the two trees beside it exist: a session that
    /// wrote its output and its working files in here would be leaving them in somebody's
    /// project, and the client would be left to work out which files were new.
    ///
    /// So this tree is for *reading*, which is what its name says, what the two members
    /// beside it exist to make possible, and what a server is expected to hold a client to:
    /// a `write` naming a path in here is refused with
    /// [`IO_FAILED`](crate::console::Error::IO_FAILED), the code a read-only filesystem
    /// already answers one with.
    ///
    /// How far that reaches is the backend's, because it is a property of what the tree is
    /// mounted as rather than of this protocol. A backend with a kernel of its own mounts it
    /// read-only and every write fails, a command's included; a backend running commands on
    /// the host can only answer for the calls it performs itself, and says so.
    ///
    /// Answered by a [`TreeMount`](super::TreeMount) saying where the server put it,
    /// which is what makes every later path in this protocol a path both ends can spell.
    /// See [`InitResp`](super::InitResp).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<TreeSource>,

    /// Where this session leaves what it produced, named by URL. `None` is a session that
    /// produces nothing anybody collects.
    ///
    /// **Apart from the context because the client's intention for it is different.** What a
    /// session was given and what it was asked to make are two sets of files with two
    /// futures: the first is somebody's project and is read, the second is the result and is
    /// collected. A session that wrote its output into the tree it was given leaves the
    /// client to work out which files are new, which is a question the client should not
    /// have to ask — and on a backend where the context is a store that is expensive or
    /// unwise to write to, it is a question with no good answer at all.
    ///
    /// Answered in [`InitResp::artifacts`](super::InitResp::artifacts) with where it went.
    /// A scheme this build has no provider for is refused at `init` with
    /// [`UNSUPPORTED_ARTIFACTS`](crate::console::Error::UNSUPPORTED_ARTIFACTS), which is
    /// [`UNSUPPORTED_CONTEXT`](crate::console::Error::UNSUPPORTED_CONTEXT)'s reasoning applied
    /// to this member and a code of its own so that a client hears *which* tree the build
    /// cannot take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<TreeSource>,
}

/// A tree a session is given, named by URL.
///
/// One type for both of them — the [`context`](InitCall::context) and the
/// [`artifacts`](InitCall::artifacts) — because naming a tree is one operation and reading
/// that name is one rule. **What a tree is *for* is the member it arrives under**, and
/// nothing about that changes how a URL is read, which kinds a build can realize, or what a
/// server does with the answer.
///
/// A type per member would have been a copy of [`scheme`](Self::scheme) and
/// [`file_path`](Self::file_path) each, and therefore a chance for two servers to disagree
/// about what one URL names — which is the failure a shared protocol type exists to
/// prevent.
///
/// A *context* is the first of them: what the session is given to work from, as against the
/// rootfs its commands run on. This is the protocol's way of naming one — not the tree
/// itself, which is a thing in some process, but where to get it.
///
/// The work is not done in it: a session writes on the root its commands run on and leaves
/// its result in its [`artifacts`](InitCall::artifacts), and what this tree is for is being
/// *read*.
///
/// # The scheme is the kind
///
/// | scheme | is |
/// |---|---|
/// | `file:///srv/project` | a directory on the server's own filesystem |
/// | `http://…`, `https://…` | a tree reached over HTTP — **on the wire, implemented nowhere** |
///
/// A scheme this build has no provider for is refused at `init`, naming it — with the code
/// belonging to the member it arrived under, so that a client asking for both trees hears
/// which one the build cannot take:
/// [`UNSUPPORTED_CONTEXT`](crate::console::Error::UNSUPPORTED_CONTEXT),
/// [`UNSUPPORTED_ARTIFACTS`](crate::console::Error::UNSUPPORTED_ARTIFACTS). That is what
/// `http` and `https` get everywhere today.
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
/// member rather than the whole of a tree, and what carries a credential is a member
/// beside it, added when there is a provider that reads one. `file://` needs none, which is
/// why there is none here yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeSource {
    /// `file:///srv/project`, `https://example.com/share`.
    pub url: String,
}

impl TreeSource {
    pub fn new(url: impl Into<String>) -> Self {
        TreeSource { url: url.into() }
    }

    /// The scheme, which is the kind — `"file"`, `"https"`, or the whole URL when it has
    /// no `://` in it and so names no kind at all.
    ///
    /// What a server branches on to decide whether it has a provider, and what it names in
    /// the refusal when it has not — see the type's docs for which code that is.
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

/// Which of a session's trees a [`TreeSource`] arrived as.
///
/// The member name and the code that refuses a scheme the build cannot realize, as one
/// value — because a server that realizes trees does the same work per tree and differs
/// only in what it calls the tree and what it refuses it with. Passing this is what lets
/// that be one function.
///
/// Here rather than in each server for the reason [`file_path`](TreeSource::file_path) is
/// here: which code answers which member is a fact about the protocol, and two servers that
/// spelled it separately could disagree about it — leaving a client to branch on a code that
/// means one thing on one backend and another on the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeRole {
    /// [`InitCall::context`] — what the session is given to work from.
    Context,

    /// [`InitCall::artifacts`] — what it is to leave behind.
    Artifacts,
}

impl TreeRole {
    /// The member this tree travels under, which is also what an error calls it.
    pub fn as_str(self) -> &'static str {
        match self {
            TreeRole::Context => "context",
            TreeRole::Artifacts => "artifacts",
        }
    }

    /// What a scheme this build has no provider for is refused with.
    ///
    /// One code per member, so a client that named both trees hears which of them the
    /// build cannot take — see [`UNSUPPORTED_CONTEXT`](super::Error::UNSUPPORTED_CONTEXT).
    pub fn unsupported(self) -> i64 {
        match self {
            TreeRole::Context => super::Error::UNSUPPORTED_CONTEXT,
            TreeRole::Artifacts => super::Error::UNSUPPORTED_ARTIFACTS,
        }
    }
}

impl std::fmt::Display for TreeRole {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.as_str())
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
/// treatment an unknown context scheme gets, and for the same reason: a peer that has never
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
/// The same reason [`TreeSource`] is one. A reach is not always a single word — the ports
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
/// [`path`](super::TreeMount::path) `init` answered with — so the file this names is the
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

/// Take what this session has written so far. The `params` of `snapshot`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCall {}
