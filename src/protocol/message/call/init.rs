use std::{path::Path, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::image::ImageSource;

/// What a session is. The `params` of `init`.
///
/// What is here outlives any one execution, which is what it is doing here rather than on
/// an [`ExecCall`]: a tree has to be somewhere before a path can name a file in
/// it, and the base and the network are the environment a command runs in rather than
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
/// It is a departure from [`Directory`](crate::fs::Directory)'s composition, which is how a
/// session gets *many stores* in one tree, and the two answer different questions. Several
/// stores under one root are one namespace a command walks; these are separate namespaces
/// the client places itself.
///
/// An empty list is a session with nothing mounted, which is still a session — a command
/// then sees whatever the executor's own filesystem holds and nothing this protocol
/// described.
///
/// # The machine is asked for, and what is asked for is what is given
///
/// [`vcpus`](Self::vcpus), [`memory_mib`](Self::memory_mib), [`gpu`](Self::gpu),
/// [`gpu_memory_mib`](Self::gpu_memory_mib) and [`disk_gib`](Self::disk_gib) are the shape of
/// the thing the session runs in, and they are here rather than on an [`ExecCall`] because a
/// machine is made before the first command and outlives the last one: a backend with a
/// kernel of its own has fixed all of them before that kernel starts.
///
/// **A server provides what is named or refuses the session**, which is
/// [`network`](Self::network)'s rule applied to the rest of the machine, and it is what makes
/// them worth saying rather than measuring afterwards. A session quietly given two vCPUs
/// where it asked for eight, or no accelerator where it asked for one, is not a narrower
/// session — it is a client drawing conclusions from how long its commands took, about a
/// machine nothing ever told it the shape of. A shape this server cannot make is
/// [`UNSUPPORTED_MACHINE`](crate::console::Error::UNSUPPORTED_MACHINE), said at `init` while
/// the client can still ask for something else.
///
/// **Each is separately optional, and `None` is the common case.** Leaving one out is not a
/// default this file names: it is the server's own, which is the only end that knows what the
/// host it runs on has. So a client with an opinion about memory and none about the rest says
/// one member and the machine is otherwise whatever that server makes.
///
/// There is no member for what a *command* gets — a share of the machine, an affinity, a
/// limit. The machine is the unit of what this protocol hands out, and a session that wants
/// two sizes of it is two sessions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitCall {
    /// The base a session's commands run in.
    ///
    /// It is essential if it runs on VM environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSource>,

    /// Optional snapshot of what an earlier session changed from the base image — what a
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

    /// Whether the session's commands reach a network at all. `None` leaves it to the server.
    ///
    /// **Said once, for the same reason the trees are.** What a command can reach is a
    /// property of the environment it runs in — on some backends a device that has to be
    /// attached before a kernel comes up — so it cannot be decided per `exec` without meaning
    /// a different session for every command.
    ///
    /// **On or off, and nothing between.** On is what a process on the server's machine
    /// reaches, less that machine's own loopback: the services listening there are the
    /// operator's, and a session is let at one of them by nothing but the operator running it
    /// somewhere a session could reach anyway. Off is no network at all. A level between the
    /// two would be a firewall this protocol described and every backend reimplemented, and
    /// where a session must be kept off some part of a network, the server's machine is the
    /// place that already knows how.
    ///
    /// A value the server cannot honour is
    /// [`UNSUPPORTED_NETWORK`](crate::console::Error::UNSUPPORTED_NETWORK) — a server whose
    /// commands run on this host cannot take the network away from them, so it refuses
    /// `false` rather than taking it and running them anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,

    /// Ports on the server's machine that lead into the session, each one spelled the way
    /// docker's `-p` spells it — see [`Port`]. Empty publishes none.
    ///
    /// **Only in, and only TCP.** A session's commands reach out through
    /// [`network`](Self::network) and through nothing here; what this adds is a way *in*,
    /// which no amount of reaching out gives, and which a program in the session serving
    /// something — a VNC server, a dev server, a notebook — is useless without.
    ///
    /// Meaningless without a network, so a session that turns it off and names ports is a
    /// contradiction a server refuses with
    /// [`INVALID_PARAMS`](crate::console::Error::INVALID_PARAMS) rather than a list it quietly
    /// drops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<Port>,

    /// How many vCPUs the session's machine gets. `None` leaves the number to the server.
    ///
    /// **Said once, for the same reason the reach is.** A vCPU count is settled when a
    /// machine is made — on a backend with a kernel of its own, before that kernel is
    /// started — so a number on an `exec` would be a number that could only be honoured by
    /// making a different session out from under the command that asked for it.
    ///
    /// `0` is a machine nothing can run on, and is
    /// [`INVALID_PARAMS`](crate::console::Error::INVALID_PARAMS) rather than a way to spell
    /// leaving it out — which is what `None` already is, and what nearly every client wants:
    /// the end that knows what the host it runs on can spare is the server, not the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u8>,

    /// How much memory the session's machine gets, in mebibytes. `None` leaves it to the
    /// server.
    ///
    /// **The unit is in the name because a number this size implies none.** Bytes, MiB and
    /// GiB are all readings a person writing `2048` could have meant, and the two wrong ones
    /// are a machine a thousand times the size of the one that was asked for — so the member
    /// says which, the way [`timeout_ms`](ExecCall::timeout_ms) does.
    ///
    /// `0` is [`INVALID_PARAMS`](crate::console::Error::INVALID_PARAMS), for the reason a
    /// vCPU count of zero is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,

    /// Whether the session's commands get a GPU. `None` leaves it to the server.
    ///
    /// **A boolean and not a device**, because what a protocol can hold a server to here is
    /// that a command finds an accelerator and not which one it finds: what gets attached is
    /// the backend's — a virtio-gpu carrying Vulkan on one, whatever the host has on
    /// another — and a member naming a model or an API would be a promise only that
    /// backend's build could keep, on a wire schema that grew with every vendor. A session
    /// that needs a particular device asks the session: the command that would use it is the
    /// one that can see what is there.
    ///
    /// **`false` is not the same as saying nothing.** `None` is the server's choice, which is
    /// what a client with no opinion sends and what every client sent before this member
    /// existed; `false` is a session that must not have one, which is worth being able to say
    /// on a backend that would otherwise give one — a device, a renderer and the boot time
    /// they cost are not free to a session that will never open it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<bool>,

    /// How much memory the session's GPU may hold, in mebibytes. `None` leaves it to the
    /// server.
    ///
    /// **Beside [`memory_mib`](Self::memory_mib), not a share of it.** What the accelerator
    /// holds is memory of its own on one backend and the host's on another -- on a GPU that
    /// shares the host's memory, every buffer a command maps is host memory the machine's RAM
    /// does not count -- and a session that fills both has taken the two together.
    ///
    /// **Given as asked, and what the commands see.** A server attaches an accelerator whose
    /// memory is this size -- the device the commands enumerate reports it, so a program
    /// sizes itself to what it has rather than finding out at the allocation that fails --
    /// or refuses with [`UNSUPPORTED_MACHINE`](crate::console::Error::UNSUPPORTED_MACHINE),
    /// the way it refuses a GPU it has none of.
    ///
    /// A size is a property of an accelerator, so it is only something to say about a session
    /// that has one: beside a [`gpu`](Self::gpu) of `false`, or of `None` on a server that
    /// gives none, it is [`INVALID_PARAMS`](crate::console::Error::INVALID_PARAMS). So is `0`,
    /// for the reason a vCPU count of zero is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_memory_mib: Option<u32>,

    /// How much the session's commands may write, in gibibytes. `None` leaves it to the
    /// server.
    ///
    /// **Room for writes, not the size of the image.** What the base ships is not counted:
    /// this bounds what the session adds on top of it -- every file its commands create or
    /// change, and a [`snapshot`](Self::snapshot) handed back at `init` along with them --
    /// and a command that writes past it finds a full disk.
    ///
    /// **A ceiling and not an allocation.** A server need not set this much aside up front,
    /// so a session that asks for more room than it will fill costs the host what it wrote
    /// and not what it asked for. A size a server cannot give is
    /// [`UNSUPPORTED_MACHINE`](crate::console::Error::UNSUPPORTED_MACHINE).
    ///
    /// The unit is gibibytes rather than the mebibytes memory is said in because a disk is
    /// sized in them: a mebibyte is too fine a step to mean anything here. `0` is
    /// [`INVALID_PARAMS`](crate::console::Error::INVALID_PARAMS), for the reason a vCPU count
    /// of zero is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gib: Option<u32>,

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

/// A port on the server's machine that leads into the session: `<host>:<console>`.
///
/// ```text
/// "8080:80"   127.0.0.1:8080 on the server's machine reaches port 80 in the session
/// ```
///
/// **Docker's spelling, host first**, because it is the one a person writing a port has
/// already written somewhere, and a second order for the same two numbers is a session
/// published backwards. What is left out of docker's is what would be a lie here: no address
/// in front, because the port is on loopback and nowhere else, and no `/udp` after, because
/// a port is TCP.
///
/// # Loopback, and held for the whole session
///
/// The server listens at `127.0.0.1:<host>` from `init` until the session ends, across every
/// `stop` and the boot after it, so a program that was given the port keeps it. A connection
/// that arrives while nothing is booted waits for the next boot rather than being refused.
///
/// A connection reaches whatever in the session listens on `console`, whichever address it
/// listens on — its own loopback included — and is refused, as a machine on a network would
/// refuse it, when nothing does.
///
/// # Both numbers, always
///
/// The host port is the client's to choose, like every other part of a session, so there is
/// nothing for the server to answer about it: the port a client connects to is the one it
/// wrote. A port of `0` on either side is no port at all, and is refused -- a host port that
/// is taken is refused too, at `init`, while the client can still pick another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Port {
    /// The port on the server's machine a connection is made to.
    pub host: u16,

    /// The port in the session a connection reaches.
    pub console: u16,
}

impl Port {
    /// `console` in the session, reached at `host` on the server's machine.
    pub fn new(host: u16, console: u16) -> Self {
        Port { host, console }
    }
}

impl std::fmt::Display for Port {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.console)
    }
}

impl FromStr for Port {
    type Err = InvalidPort;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let invalid = |why| InvalidPort {
            why,
            spec: spec.to_string(),
        };
        let number = |part: &str| {
            part.parse::<u16>()
                .map_err(|_| invalid("not a port number"))
        };
        let (host, console) = spec
            .split_once(':')
            .ok_or_else(|| invalid("not <host>:<console>"))?;
        let port = Port::new(number(host)?, number(console)?);
        if port.host == 0 || port.console == 0 {
            return Err(invalid("port 0 is no port"));
        }
        Ok(port)
    }
}

impl Serialize for Port {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Port {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spec = String::deserialize(deserializer)?;
        spec.parse().map_err(de::Error::custom)
    }
}

/// Why a string is not a [`Port`], with the string it was reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidPort {
    why: &'static str,
    spec: String,
}

impl std::fmt::Display for InvalidPort {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}: {:?}", self.why, self.spec)
    }
}

impl std::error::Error for InvalidPort {}

/// What the server made of the session. The `result` of `init`.
///
/// Answered rather than left to a notification because this is the one thing about a
/// session a client can hear before it asks for work — that there is a server on the far
/// end, that it read the frame, that it speaks this protocol, and that it has taken what it
/// was told.
///
/// **It says nothing about where the trees went, because the call already did.** A
/// [`MountSpec`](super::MountSpec) carries the path its tree appears at, so every path in
/// the session is settled by the end that is going to spell them and there is nothing here
/// to read back. What is left is the one fact about a session the client could not have
/// worked out from what it sent: where it stands.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitResp {
    /// Where the session stands to begin with — the working directory the base image
    /// declared, and whatever the server stands a session in when it declared none.
    ///
    /// **The image, because the image is what the session runs on.** An image that says
    /// where a process starts is describing the thing it was built to run, and a session
    /// that stood somewhere else would be one where that image's own instructions are
    /// wrong. Standing in a tree the client named instead would make that tree the default
    /// destination of every relative path a command writes — somebody's project, when that
    /// is what the tree is, and a mount the client asked to be read-only at that.
    ///
    /// Which is a convention and not a rule this protocol enforces: it is one member saying
    /// one thing, and a server that stands somewhere else says so here and is read.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A port is docker's string on the wire, host first, with both numbers said.
    #[test]
    fn a_port_is_spelled_the_way_docker_spells_it() {
        assert_eq!("8080:80".parse(), Ok(Port::new(8080, 80)));
        assert_eq!(
            serde_json::to_value(Port::new(5901, 5900)).unwrap(),
            serde_json::json!("5901:5900")
        );
        assert_eq!(
            serde_json::from_value::<Port>(serde_json::json!("8080:80")).unwrap(),
            Port::new(8080, 80)
        );

        for bad in [
            "", "80", "80:", ":80", "0:80", "8080:0", "http", "70000:1", "1:2:3",
        ] {
            assert!(bad.parse::<Port>().is_err(), "{bad:?}");
        }
    }

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

    /// The machine's shape is five members and each is absent unless it was asked for, so a
    /// client with no opinion sends the frame it always sent.
    #[test]
    fn a_session_says_only_the_shape_it_asked_for() {
        let quiet = bson::serialize_to_bson(&InitCall::default()).unwrap();
        assert_eq!(quiet, bson::bson!({}));

        let init = InitCall {
            vcpus: Some(4),
            memory_mib: Some(4096),
            // Explicitly none, which is a thing to say and not a thing to leave out.
            gpu: Some(false),
            ..InitCall::default()
        };
        let wire = bson::serialize_to_bson(&init).unwrap();
        assert_eq!(
            wire,
            bson::bson!({ "vcpus": 4, "memory_mib": 4096i64, "gpu": false })
        );
        assert_eq!(bson::deserialize_from_bson::<InitCall>(wire).unwrap(), init);

        let init = InitCall {
            gpu: Some(true),
            gpu_memory_mib: Some(8192),
            ..InitCall::default()
        };
        let wire = bson::serialize_to_bson(&init).unwrap();
        assert_eq!(
            wire,
            bson::bson!({ "gpu": true, "gpu_memory_mib": 8192i64 })
        );
        assert_eq!(bson::deserialize_from_bson::<InitCall>(wire).unwrap(), init);

        let init = InitCall {
            disk_gib: Some(32),
            ..InitCall::default()
        };
        let wire = bson::serialize_to_bson(&init).unwrap();
        assert_eq!(wire, bson::bson!({ "disk_gib": 32i64 }));
        assert_eq!(bson::deserialize_from_bson::<InitCall>(wire).unwrap(), init);
    }
}
