//! What a build is called: the digest of the recipe that makes it.
//!
//! # What is in it, and what is deliberately not
//!
//! The rule is **what the build declares is hashed.** The base as the caller spelled it, the
//! steps in order, and the content of everything a `COPY` reads.
//!
//! `/abin` is not in it. A session runs cortex's own executables and no others, so there is
//! nothing there for a build to declare — and hashing what cortex ships would make a release
//! invalidate every image anyone has built.
//!
//! The network policy is not in it either. That is how a build was made rather than what it
//! is, and a failed build commits nothing, so no wrong entry can be cached by having had more
//! reach than the next attempt.
//!
//! # The trap worth knowing
//!
//! A **tag base moves.** `python:3.13` names something different next month, and this digest
//! sees only the string — so the id is the same and the old image keeps being served.
//! Hashing the resolved registry digest instead would mean a network round trip before an id
//! could be answered, which is exactly the property that makes the cache probe cheap.
//! [`ImageSource`](crate::console::ImageSource) already tells callers to prefer a digest for
//! the same underlying reason; this repeats that advice rather than solving it.

use std::fmt;
use std::path::Path;

use anyhow::Context as _;
use sha2::{Digest, Sha256};

use super::{Recipe, Step};

/// The version of what goes into a [`BuildId`].
///
/// First thing fed, so changing what the digest covers changes every id rather than silently
/// giving two different meanings to one hash. A cache full of `cortex-rootfs-1` entries stays
/// valid and simply stops being consulted.
const HASH_VERSION: &str = "cortex-rootfs-1";

/// What a build is called, and what its image is named by.
///
/// Spelled as a digest — `sha256:<hex>` — because that is what goes on the wire: a built
/// image is an OCI reference under a reserved host, and the digest is its tail.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BuildId(String);

impl BuildId {
    /// The whole spelling, `sha256:` and all.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The digest of `recipe`, whose `COPY` sources are read relative to `context`.
///
/// A function rather than a method, which is what lets the digest be tested without a
/// builder — and it takes the context beside the recipe rather than inside it, because a
/// recipe is deliberately free of anything machine-local. See [`Recipe`].
///
/// Fails only for a `COPY` source it could not read. Inventing a digest for one would let two
/// different builds share a cache entry, which is the one mistake a content-addressed cache
/// must not make.
pub(crate) fn digest(recipe: &Recipe, context: &Path) -> anyhow::Result<BuildId> {
    let mut hasher = Sha256::new();
    feed(&mut hasher, HASH_VERSION.as_bytes());
    feed(&mut hasher, recipe.base.as_bytes());

    for step in &recipe.steps {
        match step {
            Step::Run(command) => {
                feed(&mut hasher, b"run");
                feed(&mut hasher, command.as_bytes());
            }
            Step::Copy { src, dst } => {
                // Checked here as well as where the step goes on the wire, because this is
                // the earlier of the two and the answers have to agree: a source outside the
                // context exists on this side and not in the session, so hashing it would
                // give an id to a build that can never succeed.
                super::inside_context(src)?;
                feed(&mut hasher, b"copy");
                feed(&mut hasher, path_bytes(src));
                feed(&mut hasher, dst.as_bytes());
                tree(&mut hasher, context, src)?;
            }
            Step::Env { key, value } => {
                feed(&mut hasher, b"env");
                feed(&mut hasher, key.as_bytes());
                feed(&mut hasher, value.as_bytes());
            }
            Step::Workdir(dir) => {
                feed(&mut hasher, b"workdir");
                feed(&mut hasher, dir.as_bytes());
            }
        }
    }

    Ok(BuildId(format!("sha256:{:x}", hasher.finalize())))
}

/// One field, length-prefixed.
///
/// The prefix is the whole reason this is a function: concatenated fields have no boundaries,
/// so `ENV AB=C` and `ENV A=BC` would hash alike. Little-endian `u64` rather than a
/// separator, because there is no byte a path cannot contain.
fn feed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// A path as bytes, without going through `str`.
///
/// A path outside UTF-8 is still a path a build can copy, and refusing one here would be a
/// restriction the digest has no reason to impose.
fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

/// Everything under `root/relative`, in an order that does not depend on the filesystem's.
///
/// The mode goes in because `cp -a` carries it: a file that gained its executable bit is a
/// different image, and a digest that missed that would serve the old one.
fn tree(hasher: &mut Sha256, root: &Path, relative: &Path) -> anyhow::Result<()> {
    // A build context is a directory somebody points at, and this walk is a recursion over
    // it. Past the bound it is a sentence rather than a stack overflow, which is not an
    // error anyone can report — the process is simply gone, inside the call a caller was
    // told is the cheap one.
    anyhow::ensure!(
        relative.components().count() <= MAX_DEPTH,
        "{} is more than {MAX_DEPTH} directories into this build's context",
        relative.display()
    );

    let full = root.join(relative);
    let metadata = full
        .symlink_metadata()
        .with_context(|| format!("reading {} for this build's id", full.display()))?;

    feed(hasher, path_bytes(relative));

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        feed(hasher, &metadata.mode().to_le_bytes());
    }

    if metadata.is_dir() {
        feed(hasher, b"dir");
        let mut names: Vec<_> = std::fs::read_dir(&full)
            .with_context(|| format!("reading {} for this build's id", full.display()))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<_, _>>()?;
        // Sorted, because `read_dir` answers in whatever order the filesystem holds them and
        // one directory would otherwise hash two ways on two machines.
        names.sort();
        for name in names {
            tree(hasher, root, &relative.join(name))?;
        }
    } else if metadata.is_symlink() {
        // Where it points, not what is there: a symlink is copied as a symlink.
        feed(hasher, b"link");
        feed(hasher, path_bytes(&std::fs::read_link(&full)?));
    } else {
        // Streamed, not read. A build context holds whatever the caller builds from — a
        // release binary, a model, a tarball — and a digest is no reason for any of it to be
        // resident at once.
        feed(hasher, b"file");
        let mut file = std::fs::File::open(&full)
            .with_context(|| format!("reading {} for this build's id", full.display()))?;
        let length = metadata.len();
        hasher.update(length.to_le_bytes());
        let copied = std::io::copy(&mut file, hasher)
            .with_context(|| format!("reading {} for this build's id", full.display()))?;
        // The length went into the hash before the bytes, so a file that changed size while
        // it was being read would otherwise be hashed as though it were the size it started.
        anyhow::ensure!(
            copied == length,
            "{} changed while this build's id was being computed",
            full.display()
        );
    }
    Ok(())
}

/// How far into a build context the walk will go. Far past any real tree, and far short of
/// what the stack can take.
const MAX_DEPTH: usize = 256;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A recipe from the pieces a test names. The context travels beside it now rather
    /// than inside it, which is what lets the same recipe be built anywhere.
    fn recipe(base: &str, steps: &[Step]) -> Recipe {
        Recipe::new(base, steps.to_vec())
    }

    /// The same recipe twice is the same id, which is the whole point of it.
    #[test]
    fn the_same_recipe_is_the_same_id() {
        let steps = [Step::Run("apk add jq".into())];
        let here = PathBuf::from(".");
        let first = digest(&recipe("alpine:3.20", &steps), &here).unwrap();
        let again = digest(&recipe("alpine:3.20", &steps), &here).unwrap();
        assert_eq!(first, again);
        assert!(
            first.as_str().starts_with("sha256:"),
            "a build id is not spelled as a digest: {first}"
        );
    }

    /// Order is part of the recipe. `RUN a` then `RUN b` is a different filesystem from
    /// `RUN b` then `RUN a`, and a hash that could not tell them apart would serve one
    /// image for both.
    #[test]
    fn order_changes_the_id() {
        let here = PathBuf::from(".");
        let forwards = [Step::Run("a".into()), Step::Run("b".into())];
        let backwards = [Step::Run("b".into()), Step::Run("a".into())];
        assert_ne!(
            digest(&recipe("alpine", &forwards), &here).unwrap(),
            digest(&recipe("alpine", &backwards), &here).unwrap()
        );
    }

    /// The base is in it, as the caller spelled it.
    #[test]
    fn the_base_changes_the_id() {
        let steps = [Step::Run("a".into())];
        let here = PathBuf::from(".");
        assert_ne!(
            digest(&recipe("alpine:3.20", &steps), &here).unwrap(),
            digest(&recipe("alpine:3.21", &steps), &here).unwrap()
        );
    }

    /// Two fields that are adjacent in the hash must not be confusable. `ENV AB=C` and
    /// `ENV A=BC` are different instructions, and a digest that concatenated its pieces
    /// would give them one id.
    #[test]
    fn adjacent_fields_do_not_run_together() {
        let here = PathBuf::from(".");
        let one = [Step::Env {
            key: "AB".into(),
            value: "C".into(),
        }];
        let other = [Step::Env {
            key: "A".into(),
            value: "BC".into(),
        }];
        assert_ne!(
            digest(&recipe("alpine", &one), &here).unwrap(),
            digest(&recipe("alpine", &other), &here).unwrap()
        );
    }

    /// What a `COPY` reads is in it. This is the property that makes a cache entry safe to
    /// reuse: an edited file is a different build.
    #[test]
    fn changing_a_copied_file_changes_the_id() {
        let context = tempfile::tempdir().unwrap();
        std::fs::write(context.path().join("app.py"), b"print(1)").unwrap();
        let steps = [Step::Copy {
            src: "app.py".into(),
            dst: "/srv/app.py".into(),
        }];

        let before = digest(&recipe("alpine", &steps), context.path()).unwrap();
        std::fs::write(context.path().join("app.py"), b"print(2)").unwrap();
        let after = digest(&recipe("alpine", &steps), context.path()).unwrap();
        assert_ne!(before, after, "an edited file did not change the build id");
    }

    /// A directory `COPY` reads all of it, and a file added anywhere under it counts.
    #[test]
    fn changing_a_file_under_a_copied_directory_changes_the_id() {
        let context = tempfile::tempdir().unwrap();
        std::fs::create_dir(context.path().join("app")).unwrap();
        std::fs::write(context.path().join("app/main.py"), b"x").unwrap();
        let steps = [Step::Copy {
            src: "app".into(),
            dst: "/srv/app".into(),
        }];

        let before = digest(&recipe("alpine", &steps), context.path()).unwrap();
        std::fs::write(context.path().join("app/extra.py"), b"y").unwrap();
        assert_ne!(
            before,
            digest(&recipe("alpine", &steps), context.path()).unwrap()
        );
    }

    /// The executable bit is part of what `cp -a` carries, so it is part of the recipe.
    #[cfg(unix)]
    #[test]
    fn making_a_copied_file_executable_changes_the_id() {
        use std::os::unix::fs::PermissionsExt as _;

        let context = tempfile::tempdir().unwrap();
        let script = context.path().join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\n").unwrap();
        let steps = [Step::Copy {
            src: "run.sh".into(),
            dst: "/run.sh".into(),
        }];

        let before = digest(&recipe("alpine", &steps), context.path()).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(
            before,
            digest(&recipe("alpine", &steps), context.path()).unwrap()
        );
    }

    /// A `COPY` of something that is not there has no content to hash, and is said rather
    /// than hashed as if it were empty — which would give two different builds one id.
    #[test]
    fn a_copy_source_that_is_not_there_is_an_error() {
        let context = tempfile::tempdir().unwrap();
        let steps = [Step::Copy {
            src: "absent".into(),
            dst: "/absent".into(),
        }];
        let refused = digest(&recipe("alpine", &steps), context.path()).unwrap_err();
        assert!(
            refused.to_string().contains("absent"),
            "the error does not name the missing path: {refused}"
        );
    }

    /// A source that climbs out of the context is refused here rather than hashed. Only the
    /// context is mounted into the session, so such a file exists on this side and not on
    /// that one — hashing it would give an id to a build that can never succeed.
    #[test]
    fn a_copy_source_outside_the_context_is_refused() {
        let context = tempfile::tempdir().unwrap();
        let steps = [Step::Copy {
            src: "../secrets.env".into(),
            dst: "/secrets.env".into(),
        }];
        let refused = digest(&recipe("alpine", &steps), context.path())
            .unwrap_err()
            .to_string();
        assert!(refused.contains("climbs out of it"), "{refused}");
    }
}
