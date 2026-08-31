use std::path::Path;

use serde::{Deserialize, Serialize};

/// What a session is. The `params` of `init`.
///
/// What is here outlives any one execution, which is what it is doing here rather than on
/// an [`Exec`](super::Exec): the delegated names have to be in place before a command that
/// invokes one runs, and the tree has to be somewhere before a path can name a file in it,
/// so each is said once instead of on every command.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Init {
    /// Empty is not an error — a client with nothing to delegate is still a client.
    ///
    /// Meant to be sorted and free of duplicates, and **nothing checks either**. A client
    /// that repeats a name gets a session that will not boot: a backend makes one entry per
    /// name and the second collides, which arrives as `BOOT_FAILED` describing a symlink
    /// rather than the name that was said twice. It costs that client its own session and
    /// nobody else's, which is why this is written down rather than enforced — but a
    /// backend that wants to say something useful about it should check before it builds.
    ///
    /// What running one *means* is not here and cannot be: the behaviour lives in
    /// the client's [`ExecutableSet`](crate::exec::ExecutableSet), so a
    /// server only arranges for something that runs the name to reach the client —
    /// on a channel of its own, as an `exec` like any other.
    pub delegated: Vec<String>,

    /// The tree this session works in, named by URL. `None` is a session with nothing
    /// mounted, which is still a session — a command then sees whatever the executor's own
    /// filesystem holds and nothing this protocol described.
    ///
    /// Answered by a [`WorkFsMount`] saying where the server put it, which is what makes
    /// every later path in this protocol a path both ends can spell. See [`InitResult`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workfs: Option<WorkFsSource>,

    /// The base a session's commands run in, named as an OCI image. `None` leaves it to the
    /// server.
    ///
    /// **Said once, for the same reason the tree is.** What a command finds on the filesystem
    /// around it is a property of the environment it runs in, not of the command, and on a
    /// backend with a guest it is a disk attached before a kernel comes up.
    ///
    /// Answered in [`InitResult::image`] when what is in force can be named. A backend whose
    /// commands run on the server's own filesystem has no base to swap and refuses this with
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
    /// Answered in [`InitResult::network`] with what is actually in force, which is the only
    /// way a client learns what it got: `None` here is not "no network" but "the server's own
    /// choice", and a server that runs commands on the host has no choice to make.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkAccess>,
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
/// [`NetworkAccess`](crate::console::NetworkAccess) or a [`WorkFsSource`]. A reference it can
/// parse but not fetch is a different matter: pulling an image is slow enough that it belongs
/// to a boot rather than to `init`, so a name that resolves to nothing is heard from the first
/// call that needs a guest. A client that wants it sooner calls
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

/// What the server made of the session. The `result` of `init`.
///
/// Answered rather than left to a notification because this is the one thing about a
/// session a client can hear before it asks for work — that there is a server on the far
/// end, that it read the frame, that it speaks this protocol, and that it has taken what it
/// was told. It is also *where*: a session with a workfs has a path in it, and that path is
/// what every later `read`, `write` and reported [`cwd`](super::Exec::cwd) is spelled in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitResult {
    /// Where the workfs [`Init::workfs`] named went, or `None` when none was named.
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
    /// an [`Exec`](super::Exec) asking for a command says nothing about where to run it:
    /// there is one answer at any moment, the far end holds it, and an execution that moves
    /// it says so in its [`ExecResult::cwd`](super::ExecResult::cwd). This is where that
    /// state starts, and the only reading of it a client gets before running anything.
    ///
    /// Absent is a server that will not say. A client is then no worse off than it was
    /// before the field existed — every path it sends is one it built itself, and it simply
    /// cannot show where a relative one would land.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    /// The base in force, when it is one that can be named.
    ///
    /// The [`Init::image`] that was asked for, in the server's own spelling of it: a reference
    /// with no registry named is a reference to a default registry, and this is where a client
    /// learns which one that was. What makes it worth answering is the case where nothing was
    /// asked, since the base is then the server's own choice.
    ///
    /// Absent is a base with no reference to give: a server that will not say, or one running
    /// on something that is not an OCI image at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSource>,

    /// What the session's commands can actually reach.
    ///
    /// The [`Init::network`] that was asked for, when one was — a server provides that reach or
    /// refuses, so this confirms rather than negotiates. What makes it worth answering is the
    /// case where nothing was asked: the reach is then the server's own, and this is the only
    /// place a client can learn which.
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
/// A delegated executable runs in the client and has to open the file the command meant,
/// which needs one name that means one file to both ends. Two ends realizing the same
/// description separately is one way to get that, and it costs a tree on each side, a mount
/// on each side, and a rewriting step on every path that crosses — all to reconstruct
/// something one end already has.
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
/// answers at it only while something is booted — exactly like the `bin/` directory the
/// delegated names live in, and for the same reason. A client does not open it directly
/// anyway: `read`, `write` and `exec` each boot a session first, and a delegated executable
/// runs inside one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkFsMount {
    /// Absolute, and in the server's filesystem — `"/mnt/workfs"`.
    ///
    /// A relative one would be relative to a working directory nobody named, and the client
    /// has no way to guess which.
    pub path: String,
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::*;

    /// A session with nothing to mount says nothing about mounting, in either direction:
    /// the member is absent rather than null, which is what leaves room for it to mean
    /// exactly one thing when it is there.
    #[test]
    fn a_session_with_no_workfs_carries_no_workfs() {
        let init = Init {
            delegated: vec!["fetch".into()],
            ..Init::default()
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(doc, doc! {"delegated": ["fetch"]});
        assert_eq!(bson::deserialize_from_document::<Init>(doc).unwrap(), init);

        let answered = InitResult::default();
        let doc = bson::serialize_to_document(&answered).unwrap();
        assert_eq!(doc, doc! {});
        assert_eq!(
            bson::deserialize_from_document::<InitResult>(doc).unwrap(),
            answered
        );
    }

    /// The workfs is the URL and the answer is the path, which is the whole of what the two
    /// ends have to agree on about files.
    #[test]
    fn a_workfs_is_a_url_and_the_answer_is_where_it_went() {
        let init = Init {
            delegated: Vec::new(),
            workfs: Some(WorkFsSource::new("file:///srv/project")),
            image: None,
            network: None,
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(
            doc,
            doc! {"delegated": [], "workfs": {"url": "file:///srv/project"}},
        );
        assert_eq!(bson::deserialize_from_document::<Init>(doc).unwrap(), init);

        // The answer is where the tree went and where the session stands in it — the second
        // conventionally the first, which is what the two members being apart allows to not
        // be the case.
        let answered = InitResult {
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
            bson::deserialize_from_document::<InitResult>(doc).unwrap(),
            answered,
        );

        // Unknown members are ignored here as everywhere else, so a server may report more
        // about what it mounted than this reads — and a server that will not say where the
        // session stands is answered without that member rather than with a guess.
        assert_eq!(
            bson::deserialize_from_document::<InitResult>(
                doc! {"workfs": {"path": "/mnt/workfs", "kind": "local"}}
            )
            .unwrap(),
            InitResult {
                workfs: Some(WorkFsMount {
                    path: "/mnt/workfs".into(),
                }),
                cwd: None,
                image: None,
                network: None,
            },
        );
    }

    /// A base is a reference, both ways, and absent on both sides is the shape every session
    /// had before there was one to name.
    #[test]
    fn an_image_is_a_reference_and_the_answer_is_the_one_in_force() {
        let init = Init {
            delegated: Vec::new(),
            workfs: None,
            image: Some(ImageSource::new("python:3.13-slim")),
            network: None,
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(
            doc,
            doc! {"delegated": [], "image": {"reference": "python:3.13-slim"}}
        );
        assert_eq!(bson::deserialize_from_document::<Init>(doc).unwrap(), init);

        // **The answer is the server's spelling and not an echo.** A reference naming no
        // registry names a default one, and which default that was is the thing a client could
        // not have worked out for itself.
        let answered = InitResult {
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
            bson::deserialize_from_document::<InitResult>(doc).unwrap(),
            answered,
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
        let quiet = Init {
            delegated: vec!["report".into()],
            workfs: None,
            image: None,
            network: None,
        };
        assert_eq!(
            bson::serialize_to_document(&quiet).unwrap(),
            doc! {"delegated": ["report"]},
        );
        assert_eq!(
            bson::deserialize_from_document::<Init>(doc! {"delegated": ["report"]}).unwrap(),
            quiet,
        );
    }

    /// A reach is a name, both ways, and absent on both sides is the shape every session had
    /// before there was one to name.
    #[test]
    fn a_network_is_a_name_and_the_answer_is_the_one_in_force() {
        let init = Init {
            delegated: Vec::new(),
            workfs: None,
            image: None,
            network: Some(NetworkAccess::public()),
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(doc, doc! {"delegated": [], "network": {"reach": "public"}});
        assert_eq!(bson::deserialize_from_document::<Init>(doc).unwrap(), init);

        let answered = InitResult {
            workfs: None,
            cwd: None,
            image: None,
            network: Some(NetworkAccess::host()),
        };
        let doc = bson::serialize_to_document(&answered).unwrap();
        assert_eq!(doc, doc! {"network": {"reach": "host"}});
        assert_eq!(
            bson::deserialize_from_document::<InitResult>(doc).unwrap(),
            answered,
        );

        // **A session that says nothing about a network still serializes to what it always
        // did**, which is what lets a client of this build talk to a server that predates the
        // member — and a server of this build answer a client that does.
        let quiet = Init {
            delegated: vec!["report".into()],
            workfs: None,
            image: None,
            network: None,
        };
        assert_eq!(
            bson::serialize_to_document(&quiet).unwrap(),
            doc! {"delegated": ["report"]},
        );
        assert_eq!(
            bson::deserialize_from_document::<Init>(doc! {"delegated": ["report"]}).unwrap(),
            quiet,
        );
        assert_eq!(
            bson::deserialize_from_document::<InitResult>(doc! {}).unwrap(),
            InitResult::default(),
        );
    }

    /// A grant travels beside the name, and a session without one serializes to what it always
    /// did — which is what lets this member arrive without every existing peer noticing.
    #[test]
    fn a_host_port_grant_is_said_beside_the_reach() {
        let init = Init {
            delegated: Vec::new(),
            workfs: None,
            image: None,
            network: Some(NetworkAccess::host().with_host_ports([8080, 3000])),
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(
            doc,
            doc! {"delegated": [], "network": {"reach": "host", "host_ports": [8080, 3000]}}
        );
        assert_eq!(bson::deserialize_from_document::<Init>(doc).unwrap(), init);

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
}
