//! One instruction of a build.
//!
//! A step is what the caller declared, not what went on the wire: `RUN` becomes an argv with
//! the accumulated environment in front of it, and `ENV` becomes nothing at all. Keeping the
//! declared form is what lets [`BuildId`](super::BuildId) be the digest of a *recipe* — two
//! callers who declared the same thing get the same image whatever the wire did.

use std::fmt;
use std::path::PathBuf;

/// One instruction of a build.
///
/// The four the design settled on, and no others. Anything a Dockerfile can say that is not
/// one of these is warned about or refused by the adapter rather than represented here — see
/// [`Rootfs::from_dockerfile`](super::Rootfs::from_dockerfile).
#[derive(Clone, Debug, PartialEq, Eq)]
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

impl fmt::Display for Step {
    /// As the instruction it came from. `on_step` hands this to a caller that is showing a
    /// build's progress, and the line the caller wrote is what it will recognise.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Run(command) => write!(f, "RUN {command}"),
            Step::Copy { src, dst } => write!(f, "COPY {} {dst}", src.display()),
            Step::Env { key, value } => write!(f, "ENV {key}={value}"),
            Step::Workdir(dir) => write!(f, "WORKDIR {dir}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `on_step` prints. A step is shown as the instruction it came from, because
    /// that is what a caller watching a build recognises — its own line, or a
    /// Dockerfile's.
    #[test]
    fn a_step_shows_as_the_instruction_it_came_from() {
        assert_eq!(Step::Run("apk add jq".into()).to_string(), "RUN apk add jq");
        assert_eq!(
            Step::Copy {
                src: "app".into(),
                dst: "/srv/app".into()
            }
            .to_string(),
            "COPY app /srv/app"
        );
        assert_eq!(
            Step::Env {
                key: "TZ".into(),
                value: "UTC".into()
            }
            .to_string(),
            "ENV TZ=UTC"
        );
        assert_eq!(Step::Workdir("/srv".into()).to_string(), "WORKDIR /srv");
    }
}
