use serde::{Deserialize, Serialize};

use super::ImageSource;

/// Keep what this session has written, as a base a later session can name. The `params` of
/// `commit`.
///
/// # The name is the client's
///
/// A server does not decide what an image is called, because it does not know what the image
/// *is*: it was made out of steps only the client has, and two clients that ran the same ones
/// should land on the same image without either asking the other. So the name arrives already
/// chosen, and the server's part is to have it afterwards.
///
/// # Only what the session wrote
///
/// Not the whole filesystem. The base it started from is already somewhere, and a commit that
/// copied it would cost the size of that base every time. What is kept is the difference: the
/// files this session created or replaced, and markers for what it deleted.
///
/// # The session has to have said it might
///
/// Refused unless [`Init::committable`](super::Init::committable) was set. On a backend with a
/// guest, being *able* to answer this is something arranged while booting — and a boot has
/// long happened by the time this arrives.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    /// What the image will be called by, as the client chose it.
    pub id: String,

    /// What the built image should state about running a process in it — `KEY=value`, the
    /// spelling an image config uses.
    ///
    /// **Stated and not observed.** A session's own environment holds things that belong to
    /// the session rather than to the image, and only the client knows which of them it meant
    /// to keep.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,

    /// Where a process in the built image should start, or `None` to say nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
}

/// What the commit made. The `result` of `commit`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitResult {
    /// The image, spelled the way a later session would name it.
    ///
    /// Answered rather than left for the client to build, for the reason
    /// [`InitResult`](super::InitResult) answers where the workfs went: how a server spells
    /// something it made is the server's, and a client assembling the string itself would be
    /// a second place that has to agree.
    pub image: ImageSource,
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::*;

    #[test]
    fn a_commit_carries_the_name_it_will_be_known_by() {
        let commit = Commit {
            id: "sha256:abc".into(),
            env: vec!["TZ=UTC".into()],
            working_dir: Some("/srv".into()),
        };
        let doc = bson::serialize_to_document(&commit).unwrap();
        assert_eq!(
            bson::deserialize_from_document::<Commit>(doc).unwrap(),
            commit
        );
    }

    /// A commit that states nothing about running a process says nothing, rather than saying
    /// empty — the rule every other type here follows.
    #[test]
    fn a_commit_that_states_nothing_says_nothing() {
        let commit = Commit {
            id: "sha256:abc".into(),
            env: Vec::new(),
            working_dir: None,
        };
        let written = bson::serialize_to_document(&commit).unwrap();
        assert_eq!(written, doc! {"id": "sha256:abc"});
        assert_eq!(
            bson::deserialize_from_document::<Commit>(written).unwrap(),
            commit
        );
    }

    #[test]
    fn a_result_names_the_image_that_was_made() {
        let result = CommitResult {
            image: ImageSource::new("cortex.local/built@sha256:abc"),
        };
        let written = bson::serialize_to_document(&result).unwrap();
        assert_eq!(
            bson::deserialize_from_document::<CommitResult>(written).unwrap(),
            result
        );
    }
}
