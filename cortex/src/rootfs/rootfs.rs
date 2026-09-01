//! The builder: what a caller says a build is.
//!
//! Everything here is declaration. Nothing contacts a server, nothing is read except what
//! [`id`](Rootfs::id) has to hash, and the value can be carried around and asked its id
//! before anything is started.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::console::{ExecResult, NetworkAccess};

use super::{BuildId, Recipe, Step, digest};

/// A build: a base, the steps over it, and what those steps are allowed to see.
///
/// Built by chaining — every setter takes and returns `self` — because a build is one
/// declaration and a half-configured one is not a thing worth having a name for.
///
/// What it does *not* hold is a console. A build opens its own, from the factory handed to
/// `build`, because it may open two: one to ask whether the image already exists, and one to
/// make it.
pub struct Rootfs {
    base: String,
    steps: Vec<Step>,
    /// What a `COPY` source is relative to, and what is mounted as the build session's
    /// workfs. Defaults to the current directory, which is what a caller writing
    /// `.copy("app", …)` means by `app`.
    context: PathBuf,
    network: Option<NetworkAccess>,
    warnings: Vec<Warning>,
    /// Called after each step that reached the server. Boxed because a caller's closure is
    /// its own type and this struct has no business being generic over it — a `Rootfs` is
    /// passed around and stored, and a type parameter for an observer would spread to
    /// everything holding one.
    // Read by `build`, which is the next thing written.
    #[allow(dead_code)]
    on_step: Option<Observer>,
}

/// What [`Rootfs::on_step`] holds.
///
/// A name rather than the type written out, because the type written out is long enough that
/// the field it sits in stops being readable.
type Observer = Box<dyn FnMut(&Step, &ExecResult) + Send>;

/// Something a Dockerfile said that this build did not act on.
///
/// Carried on the value as well as written to stderr, because a library that can only print
/// is a library whose behaviour cannot be asserted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// The line of the Dockerfile it was on, counting from one.
    pub line: usize,
    /// The instruction, as the Dockerfile spelled it — `USER`, `CMD`.
    pub instruction: String,
    /// What it means for this build, said in the terms of what will happen instead.
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Dockerfile:{}: {} {}",
            self.line, self.instruction, self.message
        )
    }
}

impl Rootfs {
    /// A build over an OCI base, named as a registry spells one.
    ///
    /// A tag works and a digest is better, for the reason [`BuildId`] states: this build's
    /// id sees the string, so a tag that moves keeps serving the old image.
    pub fn from_image(reference: impl Into<String>) -> Self {
        Rootfs {
            base: reference.into(),
            steps: Vec::new(),
            context: PathBuf::from("."),
            network: None,
            warnings: Vec::new(),
            on_step: None,
        }
    }

    /// A command, run through `sh -lc` with everything [`env`](Self::env) has said so far.
    pub fn run(mut self, command: impl Into<String>) -> Self {
        self.steps.push(Step::Run(command.into()));
        self
    }

    /// Something from the build context, copied into the image.
    ///
    /// `src` is relative to the [`context`](Self::context) directory; an absolute one is
    /// refused when the build runs, because it names a place the context does not contain
    /// and so is not part of what this build declared.
    pub fn copy(mut self, src: impl AsRef<Path>, dst: impl Into<String>) -> Self {
        self.steps.push(Step::Copy {
            src: src.as_ref().to_path_buf(),
            dst: dst.into(),
        });
        self
    }

    /// A variable, for every later [`run`](Self::run) and for what the image states.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.steps.push(Step::Env {
            key: key.into(),
            value: value.into(),
        });
        self
    }

    /// Where later steps run, and what the image states.
    pub fn workdir(mut self, dir: impl Into<String>) -> Self {
        self.steps.push(Step::Workdir(dir.into()));
        self
    }

    /// The directory a `COPY` reads from, and what the build session works in.
    ///
    /// Defaults to the current directory. [`from_dockerfile`](Self::from_dockerfile)
    /// defaults it to the Dockerfile's own directory instead, which is what `docker build`
    /// callers expect of a path beside their Dockerfile.
    pub fn context(mut self, dir: impl Into<PathBuf>) -> Self {
        self.context = dir.into();
        self
    }

    /// How much of a network the build's commands get.
    ///
    /// Not part of the id: this is how the build was made rather than what it is, and a
    /// build that failed for want of a network commits nothing, so no wrong entry can be
    /// cached by it.
    pub fn network(mut self, network: NetworkAccess) -> Self {
        self.network = Some(network);
        self
    }

    /// Watch each step as it finishes.
    ///
    /// Called for every step that reached the server, which is every step but
    /// [`env`](Self::env) — that one accumulates here and sends nothing. It observes and
    /// cannot change anything: the return value is dropped, and a panic in it takes the
    /// build down with it.
    pub fn on_step(mut self, f: impl FnMut(&Step, &ExecResult) + Send + 'static) -> Self {
        self.on_step = Some(Box::new(f));
        self
    }

    /// What this build is called, which is the digest of everything it declared.
    ///
    /// Answerable before anything is built, which is what makes the cache probe cheap — no
    /// registry is contacted and nothing is booted.
    ///
    /// Fails for a `COPY` source it cannot read. See [`BuildId`] for why that is an error
    /// rather than a digest of nothing.
    pub fn id(&self) -> anyhow::Result<BuildId> {
        digest(Recipe {
            base: &self.base,
            steps: &self.steps,
            context: &self.context,
        })
    }

    /// What [`from_dockerfile`](Self::from_dockerfile) skipped, in the order it met them.
    ///
    /// Already written to stderr; this is for a caller that wants to render them itself.
    /// Empty for a builder written by hand, which has nothing to skip.
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// The base, as the caller spelled it.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The steps, in order.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// What the built image should state about running a process in it: the accumulated
    /// environment as `KEY=VALUE`, and the last working directory.
    ///
    /// Computed from the steps rather than observed from a session, so the answer is the
    /// same whether the build ran or was found in the cache.
    // Read by `build`, which is the next thing written.
    #[allow(dead_code)]
    pub(crate) fn stated(&self) -> (Vec<String>, Option<String>) {
        let mut env: Vec<(&str, &str)> = Vec::new();
        let mut working_dir = None;
        for step in &self.steps {
            match step {
                Step::Env { key, value } => match env.iter_mut().find(|(k, _)| *k == key) {
                    // In the position it was first given: a repeated assignment replaces a
                    // value, it does not move the variable to the end.
                    Some(existing) => existing.1 = value,
                    None => env.push((key, value)),
                },
                Step::Workdir(dir) => working_dir = Some(dir.clone()),
                Step::Run(_) | Step::Copy { .. } => {}
            }
        }
        (
            env.into_iter().map(|(k, v)| format!("{k}={v}")).collect(),
            working_dir,
        )
    }

    /// Where the warnings live, for the adapter to fill in.
    // Read by the Dockerfile adapter, which is the next thing written.
    #[allow(dead_code)]
    pub(crate) fn warned(mut self, warnings: Vec<Warning>) -> Self {
        self.warnings = warnings;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The calls accumulate in the order they were made, which is the order they will run.
    #[test]
    fn the_calls_are_the_steps() {
        let rootfs = Rootfs::from_image("alpine:3.20")
            .run("apk add jq")
            .copy("app", "/srv/app")
            .env("TZ", "UTC")
            .workdir("/srv/app");

        assert_eq!(
            rootfs.steps(),
            [
                Step::Run("apk add jq".into()),
                Step::Copy {
                    src: "app".into(),
                    dst: "/srv/app".into()
                },
                Step::Env {
                    key: "TZ".into(),
                    value: "UTC".into()
                },
                Step::Workdir("/srv/app".into()),
            ]
        );
        assert_eq!(rootfs.base(), "alpine:3.20");
    }

    /// A builder written by hand warns about nothing. Only the Dockerfile adapter has
    /// anything to skip.
    #[test]
    fn a_hand_written_builder_warns_about_nothing() {
        assert!(
            Rootfs::from_image("alpine")
                .run("true")
                .warnings()
                .is_empty()
        );
    }

    /// What the built image will state: the environment accumulated in order, and the last
    /// working directory.
    #[test]
    fn what_the_image_will_state_is_accumulated() {
        let rootfs = Rootfs::from_image("alpine")
            .env("TZ", "UTC")
            .workdir("/one")
            .env("LANG", "C")
            .workdir("/two");
        let (env, working_dir) = rootfs.stated();
        assert_eq!(env, ["TZ=UTC", "LANG=C"]);
        assert_eq!(working_dir.as_deref(), Some("/two"));
    }

    /// A variable said twice is the last one, in the position it was first given — which is
    /// what a shell does with a repeated assignment and what a reader expects.
    #[test]
    fn a_variable_said_twice_is_the_last_one() {
        let rootfs = Rootfs::from_image("alpine")
            .env("TZ", "UTC")
            .env("LANG", "C")
            .env("TZ", "Asia/Seoul");
        let (env, _) = rootfs.stated();
        assert_eq!(env, ["TZ=Asia/Seoul", "LANG=C"]);
    }

    /// A build with nothing to copy still has an id, and does not need a context that
    /// exists to answer one.
    #[test]
    fn an_id_needs_no_context_when_nothing_is_copied() {
        let rootfs = Rootfs::from_image("alpine").run("true");
        assert!(rootfs.id().is_ok());
    }

    /// Two builders that said the same things have the same id however they were spelled.
    #[test]
    fn the_same_calls_give_the_same_id() {
        let one = Rootfs::from_image("alpine").run("true").env("A", "b");
        let other = Rootfs::from_image("alpine").run("true").env("A", "b");
        assert_eq!(one.id().unwrap(), other.id().unwrap());
    }
}
