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
/// an [`ExecCall`]: a tree has to be somewhere before a path can name a file in
/// it, and the base and the reach are the environment a command runs in rather than
/// anything a command says — so each is said once instead of on every command.
///
/// # Three trees, because they are three lifetimes
///
/// [`context`](Self::context), [`artifacts`](Self::artifacts) and [`scratch`](Self::scratch)
/// are each a [`TreeSource`] and each answered with a [`TreeMount`](super::TreeMount), and
/// what tells them apart is the member name rather than anything in the value.
///
/// | member | is | outlives the session |
/// |---|---|---|
/// | `context` | what the session is given to work **from**, and reads | yes — it was there before |
/// | `artifacts` | what the session is to leave behind | yes — that is the point of it |
/// | `scratch` | room to work in | no |
///
/// **Which is why they are three members and not one tree with three directories in it.**
/// A single tree makes the three the same thing to everyone holding it: the same store
/// behind them, the same lifetime, the same permissions, and a client that wants to keep
/// what a session produced has to know which subdirectory that was and trust the session
/// not to have written outside it. Naming them separately is what lets each be backed by
/// what it should be — a project directory, a bucket the caller collects from, a tmpfs the
/// host throws away — and it is the protocol's only way to say which of them a path is in.
///
/// It is a departure from [`ContextFs`](crate::fs::ContextFs)'s composition, which is how a
/// session gets *many stores* in one tree, and the two answer different questions. Several
/// stores under one root are one namespace a command walks; these are separate namespaces a
/// client has separate intentions for.
///
/// Each of the three is independently optional, and a session with none of them is still a
/// session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitCall {
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

    /// Room for this session to work in, named by URL. `None` is a session that works
    /// wherever it can write.
    ///
    /// **Where the session stands, and the only one of the three meant to be thrown away.**
    /// A command that unpacks an archive, builds something, or writes a file it will read
    /// back needs somewhere to put it, and the two trees above are the wrong place for
    /// different reasons: one is somebody's project and one is what the client will collect.
    ///
    /// So this is what [`InitResp::cwd`](super::InitResp::cwd) names when a session has one
    /// — see there — which is what makes it the default destination of every relative path a
    /// command writes without having to be told.
    ///
    /// Answered in [`InitResp::scratch`](super::InitResp::scratch), and refused with
    /// [`UNSUPPORTED_SCRATCH`](crate::console::Error::UNSUPPORTED_SCRATCH) by a build with no
    /// provider for its scheme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch: Option<TreeSource>,

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

    /// Credentials the session's requests carry without its commands holding them — see
    /// [`SecretAccess`]. Said here and not per `exec` because what a request may carry is fixed
    /// with the environment the commands run in, before the guest comes up. Empty leaves it to
    /// the server; a backend that runs commands on the host has no interception seam and refuses
    /// a session that asks for one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretAccess>,

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

/// A tree a session is given, named by URL.
///
/// One type for all three of them — the [`context`](InitCall::context), the
/// [`artifacts`](InitCall::artifacts) and the [`scratch`](InitCall::scratch) — because
/// naming a tree is one operation and reading that name is one rule. **What a tree is
/// *for* is the member it arrives under**, and nothing about that changes how a URL is
/// read, which kinds a build can realize, or what a server does with the answer.
///
/// Three types here would have been three copies of [`scheme`](Self::scheme) and
/// [`file_path`](Self::file_path), and therefore three chances for two servers to disagree
/// about what one URL names — which is the failure a shared protocol type exists to
/// prevent.
///
/// A *context* is the first of them: what the session is given to work from, as against the
/// rootfs its commands run on. This is the protocol's way of naming one — not the tree
/// itself, which is a thing in some process, but where to get it.
///
/// The work is not done in it: a session writes in its [`scratch`](InitCall::scratch) and
/// leaves its result in its [`artifacts`](InitCall::artifacts), and what this tree is for is
/// being *read*.
///
/// # The scheme is the kind
///
/// | scheme | is |
/// |---|---|
/// | `file:///srv/project` | a directory on the server's own filesystem |
/// | `http://…`, `https://…` | a tree reached over HTTP — **on the wire, implemented nowhere** |
///
/// A scheme this build has no provider for is refused at `init`, naming it — with the code
/// belonging to the member it arrived under, so that a client asking for three trees hears
/// which one the build cannot take: [`UNSUPPORTED_CONTEXT`](crate::console::Error::UNSUPPORTED_CONTEXT),
/// [`UNSUPPORTED_ARTIFACTS`](crate::console::Error::UNSUPPORTED_ARTIFACTS),
/// [`UNSUPPORTED_SCRATCH`](crate::console::Error::UNSUPPORTED_SCRATCH). That is what `http`
/// and `https` get everywhere today.
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
/// value — because a server that realizes trees does the same work three times and differs
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

    /// [`InitCall::scratch`] — room to work in, and where it stands.
    Scratch,
}

impl TreeRole {
    /// The member this tree travels under, which is also what an error calls it.
    pub fn as_str(self) -> &'static str {
        match self {
            TreeRole::Context => "context",
            TreeRole::Artifacts => "artifacts",
            TreeRole::Scratch => "scratch",
        }
    }

    /// What a scheme this build has no provider for is refused with.
    ///
    /// One code per member, so a client that named three trees hears which of them the
    /// build cannot take — see [`UNSUPPORTED_CONTEXT`](super::Error::UNSUPPORTED_CONTEXT).
    pub fn unsupported(self) -> i64 {
        match self {
            TreeRole::Context => super::Error::UNSUPPORTED_CONTEXT,
            TreeRole::Artifacts => super::Error::UNSUPPORTED_ARTIFACTS,
            TreeRole::Scratch => super::Error::UNSUPPORTED_SCRATCH,
        }
    }
}

impl std::fmt::Display for TreeRole {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.as_str())
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
/// [`NetworkAccess`] or a [`TreeSource`]. A reference it can parse but not fetch is a
/// different matter: pulling an image is slow enough that it belongs to a boot rather than
/// to `init`, so a name that resolves to nothing is heard from the first call that needs a
/// guest. A client that wants it sooner calls
/// [`Console::start`](crate::console::Console::start).
///
/// # Why an object holding one member
///
/// The same reason [`TreeSource`] is one. A platform, a pull policy, a place to put
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

/// A credential a session's requests carry without the session's commands ever holding it.
///
/// The client names an environment variable the *server* holds the value of, the one host it may
/// be sent to, and where in the request it goes; the server substitutes the real value on the way
/// out and the commands see only a placeholder. The value never leaves the server, which is the
/// point. A backend that runs commands on the server's own machine has no interception seam and
/// refuses it.
///
/// One host per secret: a credential for several hosts is declared once each, and a family of
/// subdomains is a `*.suffix` wildcard [`host`](Self::host), not a list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretAccess {
    /// The environment variable whose value is injected — held by the server, seen by the
    /// commands only as a placeholder.
    pub env_var: String,

    /// The host the value may be sent to. A request to any other never receives it.
    ///
    /// An exact hostname, or a `*.suffix` wildcard (`*.googleapis.com`) covering its subdomains.
    pub host: String,

    /// Where in the request the value goes.
    pub location: SecretLocation,
}

/// Where in a request a [`SecretAccess`] is injected. On the wire it is its own lower-case name
/// (`query`, `header`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretLocation {
    /// A URL query parameter, e.g. `?apiKey=<value>`.
    Query,
    /// A request header. Covers HTTP Basic too, whose value is substituted into the base64-decoded
    /// `user:password` — Basic is a header, not a location of its own.
    Header,
}

impl SecretAccess {
    /// A credential named by the variable the server holds it in, sent to `host` (an exact
    /// hostname or a `*.suffix` wildcard), injected at `location`.
    pub fn new(
        env_var: impl Into<String>,
        host: impl Into<String>,
        location: SecretLocation,
    ) -> Self {
        SecretAccess {
            env_var: env_var.into(),
            host: host.into(),
            location,
        }
    }

    /// Injected into a URL query parameter, e.g. `?apiKey=<value>`.
    ///
    /// ```
    /// # use cortex::console::SecretAccess;
    /// SecretAccess::query("OPENWEATHER_API_KEY", "api.openweathermap.org");
    /// ```
    pub fn query(env_var: impl Into<String>, host: impl Into<String>) -> Self {
        SecretAccess::new(env_var, host, SecretLocation::Query)
    }

    /// Injected into a request header (a bearer value, or HTTP Basic).
    pub fn header(env_var: impl Into<String>, host: impl Into<String>) -> Self {
        SecretAccess::new(env_var, host, SecretLocation::Header)
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
    fn a_session_with_no_context_carries_no_context() {
        let init = InitCall::default();
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(doc, doc! {});
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc).unwrap(),
            init
        );
    }

    /// The context is the URL, which is half of what the two ends have to agree on about
    /// files; where it went is [`InitResp`](super::super::InitResp)'s half.
    #[test]
    fn a_context_is_a_url() {
        let init = InitCall {
            context: Some(TreeSource::new("file:///srv/project")),
            artifacts: None,
            scratch: None,
            image: None,
            network: None,
            secrets: Vec::new(),
            committable: false,
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(doc, doc! {"context": {"url": "file:///srv/project"}},);
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc).unwrap(),
            init
        );
    }

    /// Three trees, each under its own name and each spelled the same way — which is the
    /// whole of what one [`TreeSource`] for all of them means on the wire.
    #[test]
    fn the_three_trees_are_three_members_of_one_kind() {
        let init = InitCall {
            context: Some(TreeSource::new("file:///srv/project")),
            artifacts: Some(TreeSource::new("file:///srv/out")),
            scratch: Some(TreeSource::new("file:///srv/scratch")),
            image: None,
            network: None,
            secrets: Vec::new(),
            committable: false,
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(
            doc,
            doc! {
                "context": {"url": "file:///srv/project"},
                "artifacts": {"url": "file:///srv/out"},
                "scratch": {"url": "file:///srv/scratch"},
            },
        );
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc).unwrap(),
            init
        );
    }

    /// **Each is independently optional**, and a session that names only the two new ones
    /// says only those — which is what lets a client take a context away later without the
    /// frame becoming a shape nothing reads.
    #[test]
    fn a_session_may_name_any_of_the_trees_and_not_the_others() {
        let init = InitCall {
            context: None,
            artifacts: Some(TreeSource::new("file:///srv/out")),
            scratch: Some(TreeSource::new("file:///srv/scratch")),
            image: None,
            network: None,
            secrets: Vec::new(),
            committable: false,
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(
            doc,
            doc! {
                "artifacts": {"url": "file:///srv/out"},
                "scratch": {"url": "file:///srv/scratch"},
            },
        );
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc).unwrap(),
            init
        );

        // And a session that names neither still serializes to what it always did, which is
        // what lets a client of this build talk to a server that predates both members.
        assert_eq!(
            bson::serialize_to_document(&InitCall {
                context: Some(TreeSource::new("file:///srv/project")),
                ..InitCall::default()
            })
            .unwrap(),
            doc! {"context": {"url": "file:///srv/project"}},
        );
    }

    /// A base is a reference, and absent is the shape every session had before there was
    /// one to name.
    #[test]
    fn an_image_is_a_reference() {
        let init = InitCall {
            context: None,
            artifacts: None,
            scratch: None,
            image: Some(ImageSource::new("python:3.13-slim")),
            network: None,
            secrets: Vec::new(),
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
            context: None,
            artifacts: None,
            scratch: None,
            image: None,
            network: None,
            secrets: Vec::new(),
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
            context: None,
            artifacts: None,
            scratch: None,
            image: None,
            network: Some(NetworkAccess::public()),
            secrets: Vec::new(),
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
            context: None,
            artifacts: None,
            scratch: None,
            image: None,
            network: None,
            secrets: Vec::new(),
            committable: false,
        };
        assert_eq!(bson::serialize_to_document(&quiet).unwrap(), doc! {},);
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc! {}).unwrap(),
            quiet,
        );
    }

    /// A declared secret travels as the variable, its host and where it goes — never a value —
    /// and a session with none serializes to what it always did, so the member arrives without
    /// every existing peer noticing.
    #[test]
    fn a_secret_is_named_and_never_valued() {
        let init = InitCall {
            secrets: vec![SecretAccess::query(
                "OPENWEATHER_API_KEY",
                "api.openweathermap.org",
            )],
            ..InitCall::default()
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(
            doc,
            doc! {"secrets": [{
                "env_var": "OPENWEATHER_API_KEY",
                "host": "api.openweathermap.org",
                "location": "query",
            }]}
        );
        assert_eq!(
            bson::deserialize_from_document::<InitCall>(doc).unwrap(),
            init
        );

        // No secrets is no member, so a client that lends none is a frame a server predating
        // this cannot tell from one it already answered.
        assert_eq!(
            bson::serialize_to_document(&InitCall::default()).unwrap(),
            doc! {}
        );
    }

    /// A grant travels beside the name, and a session without one serializes to what it always
    /// did — which is what lets this member arrive without every existing peer noticing.
    #[test]
    fn a_host_port_grant_is_said_beside_the_reach() {
        let init = InitCall {
            context: None,
            artifacts: None,
            scratch: None,
            image: None,
            network: Some(NetworkAccess::host().with_host_ports([8080, 3000])),
            secrets: Vec::new(),
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
        let file = TreeSource::new("file:///srv/project");
        assert_eq!(file.scheme(), "file");
        assert_eq!(file.file_path(), Some(Path::new("/srv/project")));

        let http = TreeSource::new("https://example.com/share");
        assert_eq!(http.scheme(), "https");
        assert_eq!(http.file_path(), None);

        // No scheme at all names no kind, and the whole of it is what a server has to put
        // in the message — there is nothing shorter that would say what arrived.
        let bare = TreeSource::new("/srv/project");
        assert_eq!(bare.scheme(), "/srv/project");
        assert_eq!(bare.file_path(), None);

        // An authority is not decoded away: what follows the scheme is the path, so this
        // one is relative and the caller is the end that refuses it.
        assert_eq!(
            TreeSource::new("file://srv/project").file_path(),
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
            secrets: Vec::new(),
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
