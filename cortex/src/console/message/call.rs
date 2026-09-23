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

use std::{path::Path, str::FromStr};

use bson::{Bson, doc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::rootfs::RootFs;

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
/// {"method":"init","params":{"mounts":["file:///srv/project:/work:ro"]}}
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
/// # The trees are a list, and each one says where it goes
///
/// [`mounts`](Self::mounts) is every tree the session gets. Each is a [`MountSpec`]: where
/// to get the tree, the absolute path it appears at, and whether a command may write in it.
///
/// **What a tree is *for* is the client's and is not on the wire.** A project to read and a
/// directory to leave output in are two entries that differ in their URL, their path and
/// their `ro` — which is the whole of what a server has to know to realize either, and the
/// whole of what this protocol can hold a server to. A member per purpose would be the same
/// three facts under a name that changes none of them, and would cap a session at the
/// purposes this file happened to enumerate.
///
/// So a session that is given somebody's project and leaves its result somewhere the caller
/// collects from names two trees, a session that composes six stores names six, and the
/// reason each is there is the client's own. What the protocol settles is the part both ends
/// have to agree on: which tree is at which path, and which of them a write may land in.
///
/// Room to work in is not one of them. A session already stands on a filesystem it may write
/// to and that goes away with it, so a command that unpacks an archive or builds something
/// has somewhere to put it without the client naming a tree for it — and a tree named for
/// that purpose would be one more thing to mount, place and answer for, in exchange for what
/// the session's own root already gives.
///
/// It is a departure from [`ContextFs`](crate::fs::ContextFs)'s composition, which is how a
/// session gets *many stores* in one tree, and the two answer different questions. Several
/// stores under one root are one namespace a command walks; these are separate namespaces
/// the client places itself.
///
/// An empty list is a session with nothing mounted, which is still a session — a command
/// then sees whatever the executor's own filesystem holds and nothing this protocol
/// described.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitCall {
    /// The base a session's commands run in.
    ///
    /// It is essential if it runs on VM environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rootfs: Option<RootFs>,

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
    /// **Said once, for the same reason the trees are.** What a command can reach is a
    /// property of the environment it runs in — on some backends a device that has to be
    /// attached before a kernel comes up — so it cannot be decided per `exec` without meaning
    /// a different session for every command.
    ///
    /// Answered in [`InitResp::network`](super::InitResp::network) with what is actually in
    /// force, which is the only way a client learns what it got: `None` here is not "no
    /// network" but "the server's own choice", and a server that runs commands on the host
    /// has no choice to make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkAccess>,

    /// The trees this session works in, each one named and placed by a [`MountSpec`].
    ///
    /// **In order**, and the order is what a client uses to put one tree inside another: a
    /// server realizes them as they are written, so a mount at `/work/out` that follows one
    /// at `/work` lands inside it, and the two written the other way round do not. Nothing
    /// else depends on the order.
    ///
    /// A scheme this build has no provider for is refused at `init` with
    /// [`UNSUPPORTED_MOUNT`](crate::console::Error::UNSUPPORTED_MOUNT), naming the entry — and
    /// refused there rather than deferred to the call that needs a session, because which
    /// kinds a server can realize is a fact about the *build*: taking a session whose trees
    /// can never be there would be one in which every later path is a lie.
    ///
    /// Two entries at the same path, or one whose path this server cannot use, are a
    /// malformed request and are [`INVALID_PARAMS`](crate::console::Error::INVALID_PARAMS) —
    /// the difference being that a build is what has to change for the first and the request
    /// is what has to change for these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountSpec>,
}

/// A tree a session is given: where to get it, where it appears, and whether it may be
/// written.
///
/// # The spelling
///
/// ```text
/// <host url>:<guest path>[:<option>]…
///
/// file:///srv/project:/work:ro
/// file:///srv/out:/work/out
/// ```
///
/// **One string, because a mount is one fact.** It is also the spelling a reader already
/// has — `mount`, `fstab` and every container runtime say a source, a destination and a
/// list of options in this order — so a person reading a frame, quoting one in a bug report
/// or writing one into a config file is reading the thing they already know. An object of
/// three members would be the same three facts spread over a shape that has to be built
/// before it can be said, and a wire schema that grows a member every time a mount gains an
/// option.
///
/// # The scheme is the kind
///
/// | scheme | is |
/// |---|---|
/// | `file:///srv/project` | a directory on the server's own filesystem |
/// | `http://…`, `https://…` | a tree reached over HTTP — **on the wire, implemented nowhere** |
///
/// A URL and not a tagged object, because there is exactly one thing this protocol does with
/// it: hand it to whatever realizes that kind. A tagged object would put every kind's
/// settings in this file and make the wire schema grow with the set of providers, where a
/// string leaves the schema alone and leaves each kind's spelling to the kind — a peer that
/// has never heard of a scheme still parses it, and refuses it for the reason it actually
/// has, which is that its *build* has no provider. That refusal is
/// [`UNSUPPORTED_MOUNT`](crate::console::Error::UNSUPPORTED_MOUNT), which is what `http` and
/// `https` get everywhere today.
///
/// # The guest path is the client's to choose
///
/// [`guest_path`](Self::guest_path) is where the tree appears to the session's commands,
/// said by the end that is going to spell paths under it. So every path in the session is known before `init`
/// goes out: a [`read`](ReadCall) names a file under one of these, so does a
/// [`write`](WriteCall), and so does the command that opens the same file by the same name.
/// Nothing has to be read back, and there is no moment where a client holds a tree it cannot
/// yet name a file in.
///
/// It is absolute, because a relative one would be relative to a working directory nobody
/// named and neither end could resolve.
///
/// # The options
///
/// | option | means |
/// |---|---|
/// | `ro` | the session reads this tree and does not write in it |
/// | `rw` | a command may write in it — the default, and sayable so a client can be explicit |
///
/// **Read-only is per tree, because it is a property of the mount and not of the tree's
/// purpose.** A [`write`](WriteCall) naming a path under an `ro` mount is refused with
/// [`IO_FAILED`](crate::console::Error::IO_FAILED), the code a read-only filesystem already
/// answers one with. How far that reaches is the backend's: one with a kernel of its own
/// mounts the tree read-only and a command's writes fail too, where one running commands on
/// the host can only answer for the calls it performs itself, and says so. That is what lets
/// a caller hand over somebody's project and get it back unchanged rather than a promise
/// that nothing touched it.
///
/// # How it is read
///
/// From the right: trailing colon-separated segments that are options are options, the first
/// segment from the right that begins with `/` is the guest path, and everything before it is
/// the host URL. Which is what makes a URL carrying a colon of its own — a port, say —
/// unambiguous without quoting, and what the two rules above cost: the guest path is absolute
/// and carries no colon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountSpec {
    host_url: String,
    guest_path: String,
    readonly: bool,
}

impl MountSpec {
    /// A writable mount of `host_url` at `guest_path`, or why the two do not make one.
    ///
    /// Checked here rather than at whoever sends it, so that a value of this type is one
    /// that can be written and read back as itself — see the type's docs for the two rules
    /// the spelling needs.
    pub fn new(
        host_url: impl Into<String>,
        guest_path: impl Into<String>,
    ) -> Result<Self, InvalidMount> {
        let host_url = host_url.into();
        let guest_path = guest_path.into();

        let refuse = |why| {
            Err(InvalidMount {
                why,
                spec: format!("{host_url}:{guest_path}"),
            })
        };

        match host_url.split_once("://") {
            None => return refuse("a mount URL needs a scheme"),
            Some((_, "")) => return refuse("a mount URL needs something after its scheme"),
            Some(_) => {}
        }
        if !guest_path.starts_with('/') {
            return refuse("a guest path is absolute");
        }
        if guest_path.contains(':') {
            return refuse("a guest path cannot carry a colon");
        }

        Ok(MountSpec {
            host_url,
            guest_path,
            readonly: false,
        })
    }

    /// The same mount, read-only — `ro`.
    pub fn read_only(mut self) -> Self {
        self.readonly = true;
        self
    }

    /// Where to get the tree, on the side that holds it — `file:///srv/project`.
    pub fn host_url(&self) -> &str {
        &self.host_url
    }

    /// Where it appears to the session's commands, which is what every path in the session
    /// is spelled under.
    pub fn guest_path(&self) -> &Path {
        Path::new(&self.guest_path)
    }

    /// Whether a write in this tree is refused.
    pub fn is_read_only(&self) -> bool {
        self.readonly
    }

    /// The scheme, which is the kind — `"file"`, `"https"`.
    ///
    /// What a server branches on to decide whether it has a provider, and what it names in
    /// the refusal when it has not; see the type's docs for which code that is.
    pub fn scheme(&self) -> &str {
        self.host_url
            .split_once("://")
            .map_or(&self.host_url, |(s, _)| s)
    }

    /// The directory a `file://` URL names, or `None` for any other scheme.
    ///
    /// The path is what follows the scheme, **as it stands**: nothing is percent-decoded,
    /// because a reader would then have to decode it before it was a path again, which is a
    /// second thing to get right about one directory.
    ///
    /// Whether it is absolute is the caller's to check and refuse, because that refusal is a
    /// different one: a relative path is a malformed request where an unknown scheme is a
    /// build without a provider.
    ///
    /// Here rather than in each backend because every server that realizes `file://` has to
    /// read it the same way. Two that disagree would be two servers a client cannot tell
    /// apart answering the same URL differently, which is the failure a shared protocol type
    /// exists to prevent.
    pub fn file_path(&self) -> Option<&Path> {
        self.host_url.strip_prefix("file://").map(Path::new)
    }
}

impl std::fmt::Display for MountSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}:{}", self.host_url, self.guest_path)?;
        if self.readonly {
            f.write_str(":ro")?;
        }
        Ok(())
    }
}

impl FromStr for MountSpec {
    type Err = InvalidMount;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let refuse = |why| {
            Err(InvalidMount {
                why,
                spec: spec.to_string(),
            })
        };

        // The URL's own `://` is not a separator, and neither is anything inside it, so the
        // search starts past it — which is also what makes a missing scheme the first thing
        // this refuses rather than a URL read as a path.
        let Some(scheme) = spec.find("://") else {
            return refuse("a mount URL needs a scheme");
        };
        let (head, mut rest) = spec.split_at(scheme + "://".len());

        let mut readonly = false;
        loop {
            let Some((before, last)) = rest.rsplit_once(':') else {
                return refuse("a mount needs a guest path to appear at");
            };
            // An absolute guest path is what ends the options, which is the rule that lets
            // a URL carry colons of its own.
            if last.starts_with('/') {
                let mount = MountSpec::new(format!("{head}{before}"), last)?;
                return Ok(if readonly { mount.read_only() } else { mount });
            }
            match last {
                "ro" => readonly = true,
                "rw" => readonly = false,
                // The same refusal covers a guest path that forgot its leading slash,
                // because from here the two are one thing: a trailing segment that is
                // neither an option nor a path.
                _ => return refuse("a mount trails an absolute guest path with `ro` or `rw`"),
            }
            rest = before;
        }
    }
}

impl Serialize for MountSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MountSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spec = String::deserialize(deserializer)?;
        spec.parse().map_err(de::Error::custom)
    }
}

/// Why a string is not a [`MountSpec`].
///
/// Carries the string it was reading, because a session names several trees and a peer
/// hearing only what was wrong with one of them cannot tell which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidMount {
    why: &'static str,
    spec: String,
}

impl std::fmt::Display for InvalidMount {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}: {:?}", self.why, self.spec)
    }
}

impl std::error::Error for InvalidMount {}

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
/// The same reason [`MountSpec`] is one. A reach is not always a single word — the ports
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
/// [`guest_path`](MountSpec::guest_path) of one of the session's mounts — so the file this names is the one a command would open by
/// the same name, and reading it is how a requester sees what an execution left behind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadCall {
    /// UTF-8, and the executor's own: a requester builds it by joining onto the path it
    /// named the mount at, which is the one both ends have a name for.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A mount is one string both ends read the same way, so what matters about it is that
    /// what goes out comes back as itself.
    #[test]
    fn a_mount_survives_its_spelling() {
        for spec in [
            "file:///srv/project:/work",
            "file:///srv/project:/work:ro",
            "https://example.com:8080/share:/work/in:ro",
            "file:///srv/odd:name:/work",
        ] {
            let read: MountSpec = spec.parse().expect(spec);
            assert_eq!(read.to_string(), spec);
        }

        // `rw` is the default and is sayable, so a client may be explicit — which is the one
        // spelling that does not come back as itself.
        let explicit: MountSpec = "file:///srv/project:/work:rw".parse().unwrap();
        assert_eq!(explicit.to_string(), "file:///srv/project:/work");
    }

    /// The three parts a server branches on, read out of one string.
    #[test]
    fn a_mount_says_where_the_tree_is_and_where_it_goes() {
        let mount: MountSpec = "file:///srv/project:/work:ro".parse().unwrap();

        assert_eq!(mount.host_url(), "file:///srv/project");
        assert_eq!(mount.guest_path(), Path::new("/work"));
        assert!(mount.is_read_only());
        assert_eq!(mount.scheme(), "file");
        assert_eq!(mount.file_path(), Some(Path::new("/srv/project")));

        // A scheme with no provider is still read: what refuses it is the build, not this.
        let remote: MountSpec = "s3://bucket/prefix:/work".parse().unwrap();
        assert_eq!(remote.scheme(), "s3");
        assert_eq!(remote.file_path(), None);
        assert!(!remote.is_read_only());
    }

    /// What is not a mount, and what each refusal says — a session names several trees, so
    /// each one names the string it was reading.
    #[test]
    fn what_is_not_a_mount_says_which_rule_it_broke() {
        for (spec, why) in [
            ("/srv/project:/work", "a mount URL needs a scheme"),
            (
                "file://:/work",
                "a mount URL needs something after its scheme",
            ),
            (
                "file:///srv/project",
                "a mount needs a guest path to appear at",
            ),
            (
                "file:///srv/project:work",
                "a mount trails an absolute guest path with `ro` or `rw`",
            ),
            (
                "file:///srv/project:/work:rx",
                "a mount trails an absolute guest path with `ro` or `rw`",
            ),
        ] {
            let refused = spec.parse::<MountSpec>().expect_err(spec).to_string();
            assert!(refused.starts_with(why), "{spec}: {refused}");
            assert!(refused.contains(spec), "{spec}: {refused}");
        }

        // And the same rules hold for one built rather than read, so that a value of this
        // type is always one that can be written and read back.
        assert!(MountSpec::new("file:///srv/project", "work").is_err());
        assert!(MountSpec::new("file:///srv/project", "/wo:rk").is_err());
    }

    /// The wire carries the string and nothing around it.
    #[test]
    fn a_session_names_its_trees_as_strings() {
        let init = InitCall {
            mounts: vec![
                MountSpec::new("file:///srv/project", "/work")
                    .unwrap()
                    .read_only(),
                MountSpec::new("file:///srv/out", "/work/out").unwrap(),
            ],
            ..InitCall::default()
        };

        let wire = bson::serialize_to_bson(&init).unwrap();
        assert_eq!(
            wire,
            bson::bson!({
                "mounts": ["file:///srv/project:/work:ro", "file:///srv/out:/work/out"],
            })
        );
        assert_eq!(bson::deserialize_from_bson::<InitCall>(wire).unwrap(), init);
    }
}
