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
    /// the client's [`ExecutableSet`](crate::executable::ExecutableSet), so a
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
/// answers the path on this side of that boundary and does its own translation behind it.
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
        };
        let doc = bson::serialize_to_document(&init).unwrap();
        assert_eq!(
            doc,
            doc! {"delegated": [], "workfs": {"url": "file:///srv/project"}},
        );
        assert_eq!(bson::deserialize_from_document::<Init>(doc).unwrap(), init);

        let answered = InitResult {
            workfs: Some(WorkFsMount {
                path: "/mnt/workfs".into(),
            }),
        };
        assert_eq!(
            bson::serialize_to_document(&answered).unwrap(),
            doc! {"workfs": {"path": "/mnt/workfs"}},
        );

        // Unknown members are ignored here as everywhere else, so a server may report more
        // about what it mounted than this reads.
        assert_eq!(
            bson::deserialize_from_document::<InitResult>(
                doc! {"workfs": {"path": "/mnt/workfs", "kind": "local"}}
            )
            .unwrap(),
            answered,
        );
    }
}
