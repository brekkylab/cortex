//! One instruction of a build.
//!
//! A step is what the caller declared, not what went on the wire: `RUN` becomes an argv with
//! the accumulated environment in front of it, and `ENV` becomes nothing at all. Keeping the
//! declared form is what lets a build be named by the digest of what it *declares* — two
//! callers who declared the same thing get the same image whatever the wire did.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One instruction of a build.
///
/// The four the design settled on, and no others. Anything a Dockerfile can say that is not
/// one of these is warned about or refused by the adapter rather than represented here.
/// Written under its own name — `{"run": …}`, `{"copy": {…}}` — rather than by position, so
/// a variant added later cannot change what a stored [`Image`](super::Image) means.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// A command, run through `sh -c` with the environment accumulated so far.
    Run(String),

    /// A path in the build context, copied into the image.
    ///
    /// `src` is relative to the context directory; `dst` is an absolute path in the image
    /// and is a `String` rather than a `PathBuf` because it names a place in someone else's
    /// filesystem, which this host has no business normalising.
    Copy { src: PathBuf, dst: String },

    /// A variable, added to every later [`Run`](Self::Run) and to what the built image
    /// states.
    Env { key: String, value: String },

    /// Where later steps run, and what the built image states.
    Workdir(String),
}

impl Step {
    /// A command, run through `sh -c` with everything an earlier [`env`](Self::env) said.
    pub fn run(cmd: impl Into<String>) -> Self {
        Step::Run(cmd.into())
    }

    /// Something from the build context, copied into the image.
    ///
    /// `src` is relative to the context directory; an absolute one is refused when the build
    /// runs, because it names a place the context does not contain and so is not part of
    /// what this build declared.
    pub fn copy(src: impl AsRef<Path>, dst: impl Into<String>) -> Self {
        Step::Copy {
            src: src.as_ref().to_path_buf(),
            dst: dst.into(),
        }
    }

    /// A variable, for every later [`run`](Self::run) and for what the image states.
    pub fn env(key: impl Into<String>, value: impl Into<String>) -> Self {
        Step::Env {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Where later steps run, and what the image states.
    pub fn workdir(dir: impl Into<String>) -> Self {
        Step::Workdir(dir.into())
    }
}

/// A bare string is a `RUN`.
///
/// [`Image::step`](super::Image::step) takes anything that converts, so this is what
/// decides that `.step("apk add jq")` compiles and what it means. `RUN` is the one
/// instruction whose entire declaration *is* a single string — the other three name two
/// pieces or a place — so there is nothing else a lone command could have been read as, and
/// nothing for the reader to look up.
///
/// No conversion for the others, for the same reason: `("TZ", "UTC")` could be an `ENV` or a
/// `COPY` with equal grammar, and a conversion that picked one would be picking it silently.
/// Those are spelled [`Step::env`], [`Step::copy`] and [`Step::workdir`].
impl From<&str> for Step {
    fn from(command: &str) -> Self {
        Step::run(command)
    }
}

impl From<String> for Step {
    fn from(command: String) -> Self {
        Step::Run(command)
    }
}

impl fmt::Display for Step {
    /// As the instruction it came from. A caller showing a build's progress hands this on,
    /// and the line the caller wrote is what it will recognise.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Run(command) => write!(f, "RUN {command}"),
            Step::Copy { src, dst } => write!(f, "COPY {} {dst}", src.display()),
            Step::Env { key, value } => write!(f, "ENV {key}={value}"),
            Step::Workdir(dir) => write!(f, "WORKDIR {dir}"),
        }
    }
}
