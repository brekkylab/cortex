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
pub struct CommitCall {
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
///
/// # Why the image is optional
///
/// **Two ends fill this in, and neither can fill in the other's half.** On a backend that runs
/// commands somewhere else, the end that walks the filesystem and writes the layer is inside
/// that somewhere — and it has no idea what an image *is*. It knows it wrote a tar and how big
/// the tar was. Naming the image takes a layer store and a stitch, which are the outer end's.
///
/// So the inner end answers a [`size`](Self::size) and no image, and the outer end fills the
/// image in on the way past. One shape for one method, which is what the rest of this protocol
/// does — and the `Option` says exactly which half is whose rather than leaving a member that
/// is sometimes a lie.
///
/// A client sees this only after the outer end has been through it, so an image it does not
/// carry is a backend that answered wrongly rather than a case to handle.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitResp {
    /// How big the layer this session wrote is, in bytes.
    ///
    /// Enough to tell an empty commit from a missing one, which is the difference between a
    /// session that wrote nothing and a session whose layer never arrived.
    pub size: u64,

    /// The image, spelled the way a later session would name it.
    ///
    /// Answered rather than left for the client to build, for the reason
    /// [`InitResp`](super::InitResp) answers where the context went: how a server spells
    /// something it made is the server's, and a client assembling the string itself would be
    /// a second place that has to agree.
    ///
    /// `None` only between the two ends — see above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSource>,
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::*;

    #[test]
    fn a_commit_carries_the_name_it_will_be_known_by() {
        let commit = CommitCall {
            id: "sha256:abc".into(),
            env: vec!["TZ=UTC".into()],
            working_dir: Some("/srv".into()),
        };
        let doc = bson::serialize_to_document(&commit).unwrap();
        assert_eq!(
            bson::deserialize_from_document::<CommitCall>(doc).unwrap(),
            commit
        );
    }

    /// A commit that states nothing about running a process says nothing, rather than saying
    /// empty — the rule every other type here follows.
    #[test]
    fn a_commit_that_states_nothing_says_nothing() {
        let commit = CommitCall {
            id: "sha256:abc".into(),
            env: Vec::new(),
            working_dir: None,
        };
        let written = bson::serialize_to_document(&commit).unwrap();
        assert_eq!(written, doc! {"id": "sha256:abc"});
        assert_eq!(
            bson::deserialize_from_document::<CommitCall>(written).unwrap(),
            commit
        );
    }

    #[test]
    fn a_result_names_the_image_that_was_made() {
        let result = CommitResp {
            size: 4096,
            image: Some(ImageSource::new("cortex.local/built@sha256:abc")),
        };
        let written = bson::serialize_to_document(&result).unwrap();
        assert_eq!(
            bson::deserialize_from_document::<CommitResp>(written).unwrap(),
            result
        );
    }

    /// What the inner end answers: a size, and no image, because it has none to give. The
    /// member is absent rather than null, the way the rest of this protocol leaves out what
    /// it has nothing to say about.
    #[test]
    fn the_end_that_wrote_the_layer_names_no_image() {
        let wrote = CommitResp {
            size: 4096,
            image: None,
        };
        let written = bson::serialize_to_document(&wrote).unwrap();
        assert_eq!(written.get("image"), None);
        assert_eq!(
            bson::deserialize_from_document::<CommitResp>(written).unwrap(),
            wrote
        );
    }
}
