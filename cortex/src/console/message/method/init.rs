use serde::{Deserialize, Serialize};

/// What a session is. The `params` of `init`.
///
/// What is here outlives any one execution, which is what it is doing here rather than on
/// an [`Exec`](super::Exec): the delegated names have to be in place before a command that
/// invokes one runs, so they are said once instead of on every command.
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

    /// The namespace the server is to realize, and the client realizes for itself.
    ///
    /// Here rather than on an [`Exec`](super::Exec) for the reason `delegated` is: it
    /// outlives any one execution, and a command that reads a file needs the tree to exist
    /// before it runs. It is also what a delegated executable resolves paths in — both
    /// sides build their own live tree from this one description, so a name means the same
    /// thing to the command and to the executable it invoked.
    ///
    /// **Nothing is realized by sending it.** `init` boots nothing; the first call that
    /// needs a session builds the tree, and a kind that server cannot realize is that
    /// call's failure rather than this one's.
    ///
    /// Empty is not an error. A client with nothing to mount is still a client, and then
    /// this field is absent from the frame rather than present and empty.
    #[serde(
        default,
        skip_serializing_if = "crate::volume::WorkspaceSpec::is_empty"
    )]
    pub volumes: crate::volume::WorkspaceSpec,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::{VolumeSpec, WorkspaceSpec};

    /// A client with nothing to mount sends the frame it sends today.
    #[test]
    fn an_empty_namespace_is_absent_from_the_frame() {
        let init = Init {
            delegated: vec!["fetch".into()],
            volumes: WorkspaceSpec::default(),
        };
        let doc = bson::serialize_to_document(&init).expect("serializes");
        assert_eq!(doc.get("volumes"), None);
    }

    /// Asserting on the whole document rather than probing members, which is how the
    /// neighbouring method tests do it and what makes the wire shape readable here.
    ///
    /// `bson::doc!` is qualified because this module imports only serde.
    #[test]
    fn a_declared_namespace_is_on_the_frame_and_reads_back() {
        let init = Init {
            delegated: vec![],
            volumes: WorkspaceSpec::default().mount(
                "work",
                VolumeSpec::Local {
                    host: "/tmp/p".into(),
                },
            ),
        };
        let doc = bson::serialize_to_document(&init).expect("serializes");
        assert_eq!(
            doc,
            bson::doc! {
                "delegated": [],
                "volumes": {
                    "mounts": [
                        { "path": "work", "volume": { "type": "local", "host": "/tmp/p" } }
                    ]
                }
            }
        );

        let back: Init = bson::deserialize_from_document(doc).expect("deserializes");
        assert_eq!(back, init);
    }

    /// A frame from a peer that says nothing about volumes.
    #[test]
    fn an_absent_namespace_reads_as_empty() {
        let doc = bson::doc! { "delegated": ["fetch"] };
        let init: Init = bson::deserialize_from_document(doc).expect("deserializes");
        assert!(init.volumes.is_empty());
    }
}
