//! What a build declares, as something that can be written down.
//!
//! A [`Recipe`] is the half of a [`Rootfs`](super::Rootfs) that is about the *image* — the
//! base and the steps — and nothing about the machine it is built on. That is what makes it
//! the thing worth serializing, and it is the same line the [`BuildId`](super::BuildId) is
//! drawn along: two callers who declared the same recipe get the same image, whatever their
//! filesystems look like.
//!
//! # What is deliberately not in it
//!
//! **The build context.** It is an absolute path on one machine and means nothing on
//! another, so a recipe carrying one would be a recipe that cannot travel. It lives on
//! `Rootfs`, and a recipe read back somewhere else is given one with
//! [`context`](super::Rootfs::context).
//!
//! **What a `COPY` reads.** The id hashes file *contents*, so a recipe alone does not
//! reproduce an image elsewhere — the context tree has to get there too. A recipe transports
//! the declaration, not the inputs, and it is worth saying that out loud because the two
//! look alike from a distance.

use serde::{Deserialize, Deserializer, Serialize};

use super::Step;

/// Which spelling of this format a recipe is written in.
///
/// Separate from [`HASH_VERSION`](super::id) on purpose, and the two move independently: how
/// a recipe is *written down* can change without changing what any build is called, and what
/// goes into an id can change without any stored recipe becoming unreadable.
const FORMAT_VERSION: u32 = 1;

/// A base image and the steps over it: everything a build declares, and nothing else.
///
/// Serializable, so a caller can store one beside the image it made or send one to whatever
/// will build it. The version travels with it and a newer one is refused rather than
/// half-read — see the module docs for what a recipe does *not* carry.
///
/// ```
/// # use cortex::rootfs::{Recipe, Rootfs, Step};
/// let declared = Rootfs::from_image("alpine:3.20").run("apk add jq");
///
/// let written = serde_json::to_string(declared.recipe())?;
/// let read: Recipe = serde_json::from_str(&written)?;
///
/// assert_eq!(read.base, "alpine:3.20");
/// assert_eq!(read.steps, [Step::Run("apk add jq".into())]);
///
/// // And back to something buildable, with this machine's context supplied here.
/// let rootfs = Rootfs::from_recipe(read).context("app");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # A `COPY` source outside UTF-8 cannot be written down
///
/// A build can copy one — the id hashes the path's bytes rather than going through `str`,
/// deliberately. Serializing it is what fails, because the formats this rides on have no
/// such thing as a non-UTF-8 string. It is a limit on storing a recipe and not on building
/// one, and it arrives as a serializer's error rather than silently.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    /// Written first and read first, so a recipe from a newer cortex is refused by name
    /// rather than by whatever member it happens to disagree about.
    #[serde(rename = "v", deserialize_with = "known_version")]
    version: u32,

    /// The base, as the caller spelled it.
    pub base: String,

    /// The steps, in the order they will run.
    pub steps: Vec<Step>,
}

impl Recipe {
    /// A recipe in this cortex's format.
    pub fn new(base: impl Into<String>, steps: Vec<Step>) -> Recipe {
        Recipe {
            version: FORMAT_VERSION,
            base: base.into(),
            steps,
        }
    }
}

/// Refuse a version this cortex does not speak.
///
/// A missing member is refused too, and by serde rather than here: a document with no
/// version is not a recipe, and guessing that it means version 1 would be inventing a
/// provenance for something whose provenance is the whole question.
fn known_version<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let found = u32::deserialize(d)?;
    if found != FORMAT_VERSION {
        return Err(serde::de::Error::custom(format!(
            "this recipe is written in format {found}, and this cortex reads {FORMAT_VERSION}"
        )));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_recipe() -> Recipe {
        Recipe::new(
            "alpine:3.20",
            vec![
                Step::Run("apk add jq".into()),
                Step::Copy {
                    src: "app".into(),
                    dst: "/srv/app".into(),
                },
                Step::Env {
                    key: "TZ".into(),
                    value: "UTC".into(),
                },
                Step::Workdir("/srv".into()),
            ],
        )
    }

    /// Every step survives the round trip, which is the whole point of the type.
    #[test]
    fn a_recipe_written_down_reads_back_the_same() {
        let recipe = a_recipe();
        let written = serde_json::to_string(&recipe).expect("writing it");
        assert_eq!(
            serde_json::from_str::<Recipe>(&written).expect("reading it"),
            recipe
        );
    }

    /// The version is on the wire, under a short name because it is on every recipe.
    #[test]
    fn a_recipe_says_which_format_it_is() {
        let written: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&a_recipe()).unwrap()).unwrap();
        assert_eq!(written["v"], 1);
    }

    /// A recipe from a newer cortex is refused, and says so in terms of both versions —
    /// neither alone tells the reader which side is old.
    #[test]
    fn a_recipe_from_a_newer_cortex_is_refused() {
        let refused = serde_json::from_str::<Recipe>(r#"{"v":99,"base":"alpine","steps":[]}"#)
            .expect_err("99 is not a format this speaks");
        let said = refused.to_string();
        assert!(said.contains("99"), "{said}");
        assert!(said.contains('1'), "{said}");
    }

    /// And a document with no version at all is not a recipe.
    #[test]
    fn a_document_with_no_version_is_not_a_recipe() {
        assert!(serde_json::from_str::<Recipe>(r#"{"base":"alpine","steps":[]}"#).is_err());
    }

    /// The steps are tagged by name rather than by position, so a variant added later does
    /// not change what an existing recipe means.
    #[test]
    fn a_step_is_written_under_its_own_name() {
        let written: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&a_recipe()).unwrap()).unwrap();
        let steps = written["steps"].as_array().expect("an array of steps");
        assert_eq!(steps[0], serde_json::json!({"run": "apk add jq"}));
        assert_eq!(
            steps[1],
            serde_json::json!({"copy": {"src": "app", "dst": "/srv/app"}})
        );
        assert_eq!(
            steps[2],
            serde_json::json!({"env": {"key": "TZ", "value": "UTC"}})
        );
        assert_eq!(steps[3], serde_json::json!({"workdir": "/srv"}));
    }

    /// A `COPY` source outside UTF-8 builds but does not serialize, and the failure is the
    /// serializer's rather than a path that came back changed.
    #[cfg(unix)]
    #[test]
    fn a_copy_source_outside_utf8_cannot_be_written_down() {
        use std::os::unix::ffi::OsStrExt as _;

        let recipe = Recipe::new(
            "alpine",
            vec![Step::Copy {
                src: std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"\xff")),
                dst: "/app".into(),
            }],
        );
        assert!(serde_json::to_string(&recipe).is_err());
    }
}
