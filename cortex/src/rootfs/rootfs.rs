//! The builder: what a caller says a build is.
//!
//! Everything here is declaration. Nothing contacts a server, nothing is read except what
//! [`id`](Rootfs::id) has to hash, and the value can be carried around and asked its id
//! before anything is started.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::console::{ConsoleBuilder, Error, ExecResult, Failure, ImageSource, NetworkAccess};
use crate::fs::Mount;

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

    /// A command, run through `sh -c` with everything [`env`](Self::env) has said so far.
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
    ///
    /// # This directory is writable, and it is the real one
    ///
    /// Not a copy. It is shared into the session the way any workfs is — the same directory,
    /// at the same path, over virtio-fs — so a `RUN rm -rf *` or a `RUN make` deletes and
    /// writes **the caller's own files**, and what it leaves behind changes the next build's
    /// [`id`](Self::id). `docker build` uploads a snapshot and cannot do this; nothing here
    /// makes the same promise, because the share it rides on has no read-only setting for it
    /// to be made with.
    ///
    /// So: name a directory a build may write in. A build that must not touch its input is a
    /// build whose context is a copy the caller made.
    pub fn context(mut self, dir: impl Into<PathBuf>) -> Self {
        self.context = dir.into();
        self
    }

    /// How much of a network the build's commands get.
    ///
    /// **Leaving it out is not the internet.** The choice falls to the server, and a
    /// micro-VM one defaults to reaching the host and nothing beyond it — so a `RUN` that
    /// installs a package fails at a refused connection unless this asks for
    /// [`public`](NetworkAccess::public). That is the same default an ordinary session
    /// gets, deliberately: a build is a session, and nothing about being one makes egress
    /// safer. An operator who wants it everywhere sets the server's own default instead of
    /// every caller saying it.
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
    pub(crate) fn warned(mut self, warnings: Vec<Warning>) -> Self {
        self.warnings = warnings;
        self
    }
}

/// The host a built image is named under.
///
/// `.local` is reserved by RFC 6762 and can never be a registry, so this is real OCI
/// reference grammar that no registry will ever claim. A console server checks the host
/// before anything else and serves from its own store.
pub(crate) const LOCAL_HOST: &str = "cortex.local/";

/// What a built image is called on the wire.
pub(crate) fn reference(id: &BuildId) -> String {
    format!("{LOCAL_HOST}built@{id}")
}

/// Refuse a `COPY` source that names something the build context does not contain.
///
/// Absolute is the obvious one. `..` is the other: only the context is mounted into the
/// session, so a source that climbs out of it names a directory that is empty on that side —
/// and yet [`Rootfs::id`] hashes it here, where it does exist. Left alone, the two disagree:
/// editing a file outside the context changes the build's identity, and every build carrying
/// that step fails at the `cp`. Said once, at the step that declared it.
pub(crate) fn inside_context(src: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        src.is_relative(),
        "a COPY source is relative to the build context, and {} is not",
        src.display()
    );
    anyhow::ensure!(
        !src.components()
            .any(|part| part == std::path::Component::ParentDir),
        "a COPY source is inside the build context, and {} climbs out of it",
        src.display()
    );
    Ok(())
}

/// The build context, as something a console can be given.
///
/// A directory and nothing else. [`Mount`]'s contract is about a tree that goes away when the
/// value does; this one is the caller's own directory and outlives the build, which is
/// exactly what a build context should do.
struct Context(PathBuf);

impl Mount for Context {
    fn mountpoint(&self) -> &Path {
        &self.0
    }
}

/// An image a build made.
///
/// Hand it to [`ConsoleBuilder::image`] — `From<&BuiltImage>` is why `.image(&built)` works —
/// and the next session starts where the build left off.
#[derive(Clone, Debug)]
pub struct BuiltImage {
    id: BuildId,
    image: ImageSource,
    env: Vec<String>,
    working_dir: Option<String>,
}

impl BuiltImage {
    /// The digest of the recipe that made it.
    pub fn id(&self) -> &BuildId {
        &self.id
    }

    /// How to name it to a console server.
    pub fn image(&self) -> &ImageSource {
        &self.image
    }

    /// What it states about running a process in it, as `KEY=VALUE`.
    pub fn env(&self) -> &[String] {
        &self.env
    }

    /// Where a process in it starts.
    pub fn working_dir(&self) -> Option<&str> {
        self.working_dir.as_deref()
    }
}

impl From<&BuiltImage> for ImageSource {
    fn from(built: &BuiltImage) -> ImageSource {
        built.image.clone()
    }
}

/// A step that did not succeed, with everything needed to see why.
///
/// A distinct type rather than a formatted message, so a caller can show the output its own
/// way — `anyhow::Error::downcast_ref::<StepFailed>()` is how to get at it.
#[derive(Debug)]
pub struct StepFailed {
    /// The step as the caller declared it.
    pub step: Step,
    /// What actually went on the wire, which is not the same thing — a `RUN` carries the
    /// accumulated environment in front of it.
    pub argv: Vec<String>,
    pub result: ExecResult,
}

impl fmt::Display for StepFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} exited {}\n  argv: {:?}\n  stdout: {}\n  stderr: {}",
            self.step,
            self.result.code,
            self.argv,
            String::from_utf8_lossy(&self.result.stdout),
            String::from_utf8_lossy(&self.result.stderr),
        )
    }
}

impl std::error::Error for StepFailed {}

impl Rootfs {
    /// Build it, and answer the image.
    ///
    /// # Why a factory and not a builder
    ///
    /// This may open two consoles — one to ask whether the image already exists, one to
    /// build it — and a [`ConsoleBuilder`]'s client factory is `FnOnce`, so one builder
    /// cannot serve both. Taking a server command instead would read better and would stop a
    /// test from supplying its own [`Client`](crate::console::Client).
    ///
    /// `image`, `network`, `mount` and `committable` are set on whatever the factory
    /// returns; anything the caller set for those is overwritten.
    pub async fn build(
        mut self,
        console: impl Fn() -> ConsoleBuilder,
    ) -> anyhow::Result<BuiltImage> {
        // On a blocking thread: this reads every file a `COPY` names, which is as big as
        // whatever the caller builds from. `id` itself stays synchronous, because a caller
        // asking a build its name outside a runtime is the case that makes it worth having.
        let id = {
            let (base, steps) = (self.base.clone(), self.steps.clone());
            let context = self.context.clone();
            tokio::task::spawn_blocking(move || {
                digest(Recipe {
                    base: &base,
                    steps: &steps,
                    context: &context,
                })
            })
            .await
            .context("computing this build's id")??
        };
        let (env, working_dir) = self.stated();

        if let Some(image) = already_built(&id, &console).await? {
            // Nothing was booted to find this out: `init` answered from a file test, and the
            // console it answered on has been dropped.
            return Ok(BuiltImage {
                id,
                image,
                env,
                working_dir,
            });
        }

        // Absolute, because a mount is named to the server as a `file://` URL and a relative
        // path after `file://` reads as a host. Done here rather than in `context` so that a
        // caller can name a directory that does not exist yet and be told at build time.
        let context = std::path::absolute(&self.context).with_context(|| {
            format!("resolving the build context at {}", self.context.display())
        })?;

        let mut builder = console()
            .image(self.base.clone())
            .mount(Context(context))
            .committable();
        if let Some(network) = self.network.clone() {
            builder = builder.network(network);
        }
        let mut session = builder.build().await?;

        let workfs = session
            .workfs_path()
            .context("the console server took the build context and did not say where")?
            .to_path_buf();

        let mut running: Vec<(String, String)> = Vec::new();
        for step in &self.steps {
            let argv = match step {
                Step::Env { key, value } => {
                    match running.iter_mut().find(|(k, _)| k == key) {
                        Some(existing) => existing.1 = value.clone(),
                        None => running.push((key.clone(), value.clone())),
                    }
                    // Nothing on the wire: there is no session-level environment, so this
                    // rides on every later command instead.
                    continue;
                }
                Step::Run(command) => {
                    // `env` in front rather than a shell assignment, because the argv is
                    // already split and no shell is consulted to build it — so no value
                    // needs quoting and a value with a space in it cannot become two words.
                    let mut argv = vec!["env".to_string()];
                    argv.extend(running.iter().map(|(k, v)| format!("{k}={v}")));
                    argv.push("sh".into());
                    // `-c` and not `-lc`. A login shell sources `/etc/profile`, and what
                    // `/etc/profile` does on every base worth naming is *assign* `PATH` —
                    // which throws away the one the agent built, `/abin` and the delegated
                    // names with it. A build would then be unable to run the executables it
                    // declared, while still hashing them into its own id.
                    argv.push("-c".into());
                    argv.push(command.clone());
                    argv
                }
                Step::Copy { src, dst } => {
                    inside_context(src)?;
                    let from = workfs.join(src);
                    let from = from.to_str().with_context(|| {
                        format!("{} is not a UTF-8 path to name to a server", from.display())
                    })?;
                    // The destination's parent is made first, because `COPY app /srv/app`
                    // with no `/srv` is the ordinary Dockerfile spelling — `docker build`
                    // creates the path and a bare `cp -a` does not, which would fail an
                    // instruction this adapter had already accepted.
                    //
                    // One `sh -c` rather than two steps, so a caller watching `on_step` sees
                    // the one instruction it declared.
                    vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "mkdir -p -- \"$(dirname -- \"$2\")\" && cp -a -- \"$1\" \"$2\""
                            .to_string(),
                        // `sh -c … name arg1 arg2`: the paths travel as arguments rather
                        // than inside the script, so nothing in either is ever word-split or
                        // read as a shell operator.
                        "cp".to_string(),
                        from.to_string(),
                        dst.clone(),
                    ]
                }
                // `cd` is a protocol builtin and moves the session, so it persists across the
                // steps after it rather than ending with the command.
                Step::Workdir(dir) => vec!["cd".to_string(), dir.clone()],
            };

            let result = session.exec(&argv, None).await?;
            if let Some(on_step) = self.on_step.as_mut() {
                on_step(step, &result);
            }
            if result.code != 0 {
                // Nothing is committed, so nothing is cached and the next attempt starts
                // over. Dropping the session here is what releases the guest.
                return Err(anyhow::Error::new(StepFailed {
                    step: step.clone(),
                    argv,
                    result,
                }));
            }
        }

        let image = session
            .commit(id.to_string(), env.clone(), working_dir.clone())
            .await?;

        Ok(BuiltImage {
            id,
            image,
            env,
            working_dir,
        })
    }
}

/// Whether a console server already has the image `id` names.
///
/// # Why there is no method for this
///
/// Opening a session on the image *is* the question. `init` either provides the base asked
/// for or refuses the session, which is a contract the protocol already has — so the probe
/// costs one server process and no boot, and adding a method would have meant a second way
/// to ask the same thing.
///
/// That a *built* image's existence is answered at `init` while an *OCI reference*'s is
/// deferred to boot is deliberate: the first is a local file test, the second is a network
/// round trip that belongs to a boot.
async fn already_built(
    id: &BuildId,
    console: &impl Fn() -> ConsoleBuilder,
) -> anyhow::Result<Option<ImageSource>> {
    let image = ImageSource::new(reference(id));
    match console().image(image.clone()).build().await {
        // Taken, so it is there. The console goes out of scope here, which says `quit`.
        Ok(_) => Ok(Some(image)),
        Err(e) => match e.downcast_ref::<Failure>().and_then(Failure::code) {
            Some(Error::UNKNOWN_IMAGE) => Ok(None),
            // Anything else is the caller's to hear: a server that can swap no base at all, a
            // channel that broke, a server that would not start. Building anyway would fail
            // the same way and more slowly.
            _ => Err(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use crate::BoxFuture;
    use crate::console::{
        Call, Client, CommitResult, ConsoleBuilder, Error, ExecCmd, Failure, ImageSource,
        InitResult, Notification, Outcome, Progress, RequestId, WorkFsMount,
    };

    /// Every call a build made, in order.
    type Log = Arc<Mutex<Vec<Call>>>;

    /// A client over canned answers, recording every call it was handed.
    struct Recorder {
        answers: Vec<Outcome>,
        next_id: RequestId,
        log: Log,
    }

    impl Client for Recorder {
        fn call(&mut self, call: Call) -> BoxFuture<'_, (RequestId, Result<Outcome, Failure>)> {
            let id = self.next_id;
            self.next_id += 1;
            self.log.lock().unwrap().push(call);
            let answer = if self.answers.is_empty() {
                Err(Failure::Refused(Error {
                    code: Error::INTERNAL_ERROR,
                    message: "the test ran out of answers".into(),
                    data: None,
                }))
            } else {
                Ok(self.answers.remove(0))
            };
            Box::pin(async move { (id, answer) })
        }

        fn notify(&mut self, _: Notification) -> BoxFuture<'_, Result<(), Failure>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn outcome<T: serde::Serialize>(value: &T) -> Outcome {
        Outcome::Result(bson::serialize_to_bson(value).unwrap())
    }

    /// A session taken, with the workfs put where the build said it was.
    fn took_the_session(at: &Path) -> Outcome {
        outcome(&InitResult {
            workfs: Some(WorkFsMount {
                path: at.to_str().unwrap().to_string(),
            }),
            ..InitResult::default()
        })
    }

    fn ran(code: i32) -> Outcome {
        outcome(&Progress::Done(ExecResult {
            code,
            ..ExecResult::default()
        }))
    }

    fn committed(reference: &str) -> Outcome {
        outcome(&CommitResult {
            image: ImageSource::new(reference),
        })
    }

    /// The argv of every `exec` in a log, in order.
    fn argvs(log: &Log) -> Vec<Vec<String>> {
        log.lock()
            .unwrap()
            .iter()
            .filter_map(|call| match call {
                Call::Exec(exec) => match &exec.cmd {
                    ExecCmd::New(argv) => Some(argv.clone()),
                    ExecCmd::Resume { .. } => None,
                },
                _ => None,
            })
            .collect()
    }

    /// Two consoles, in order: the first is asked for the built image, the second builds it.
    ///
    /// One helper for both because `build` may open either one console or two, and a test
    /// that had to know which would be asserting the shape of the code rather than what it
    /// sent.
    fn two(first: Vec<Outcome>, second: Vec<Outcome>) -> (impl Fn() -> ConsoleBuilder, Log) {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let queued = Arc::new(Mutex::new(vec![second, first]));
        let shared = log.clone();
        let make = move || {
            let answers = queued.lock().unwrap().pop().expect("a third console");
            ConsoleBuilder::new().client(Recorder {
                answers,
                next_id: 0,
                log: shared.clone(),
            })
        };
        (make, log)
    }

    /// The one answer that means the image is not there and the build should run.
    fn unknown_image() -> Outcome {
        Outcome::Error(Error {
            code: Error::UNKNOWN_IMAGE,
            message: "no such built image".into(),
            data: None,
        })
    }

    /// An image that already exists is answered without building it. The probe is one
    /// `init` — no boot, no steps, no commit.
    #[tokio::test]
    async fn an_image_that_exists_is_not_built_again() {
        let context = tempfile::tempdir().unwrap();
        let rootfs = Rootfs::from_image("alpine")
            .context(context.path().to_path_buf())
            .env("TZ", "UTC")
            .run("true");
        let id = rootfs.id().unwrap();

        let (make, log) = two(vec![outcome(&InitResult::default())], Vec::new());
        let built = rootfs.build(make).await.expect("the cached image");

        assert_eq!(built.id(), &id);
        assert_eq!(
            built.image().reference,
            format!("cortex.local/built@{id}"),
            "the cached image is not named the way a built one is"
        );
        // What the image states is the recipe's, not a session's — so it is the same whether
        // the build ran or was found.
        assert_eq!(built.env(), ["TZ=UTC"]);

        assert!(argvs(&log).is_empty(), "a cache hit ran a step");
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "a cache hit did more than ask"
        );
    }

    /// The probe names the built image and asks for nothing else — a session that is not a
    /// build, so it is not committable and mounts nothing.
    #[tokio::test]
    async fn the_probe_asks_only_whether_the_image_is_there() {
        let context = tempfile::tempdir().unwrap();
        let rootfs = Rootfs::from_image("alpine")
            .context(context.path().to_path_buf())
            .run("true");
        let id = rootfs.id().unwrap();

        let (make, log) = two(vec![outcome(&InitResult::default())], Vec::new());
        rootfs.build(make).await.expect("the cached image");

        let calls = log.lock().unwrap();
        let Call::Init(init) = &calls[0] else {
            panic!("the probe was not an init");
        };
        assert_eq!(
            init.image.as_ref().unwrap().reference,
            format!("cortex.local/built@{id}")
        );
        assert!(!init.committable, "the probe asked to be committable");
        assert!(init.workfs.is_none(), "the probe mounted the context");
    }

    /// An image nobody has built is built. `UNKNOWN_IMAGE` is the answer that says so, and
    /// it is the only one that means "carry on".
    #[tokio::test]
    async fn an_image_nobody_has_built_is_built() {
        let context = tempfile::tempdir().unwrap();
        let (make, log) = two(
            vec![unknown_image()],
            vec![
                took_the_session(context.path()),
                ran(0),
                committed("cortex.local/built@sha256:whatever"),
            ],
        );

        Rootfs::from_image("alpine")
            .context(context.path().to_path_buf())
            .run("true")
            .build(make)
            .await
            .expect("building");

        assert_eq!(argvs(&log), [vec!["env", "sh", "-c", "true"]]);
    }

    /// Any other refusal is the caller's to hear. A server that cannot swap a base at all
    /// answers `UNSUPPORTED_IMAGE`, and building anyway would just fail again more slowly.
    #[tokio::test]
    async fn a_refusal_that_is_not_about_the_cache_is_reported() {
        let context = tempfile::tempdir().unwrap();
        let (make, _) = two(
            vec![Outcome::Error(Error {
                code: Error::UNSUPPORTED_IMAGE,
                message: "commands here run on this host's own filesystem".into(),
                data: None,
            })],
            Vec::new(),
        );

        let refused = Rootfs::from_image("alpine")
            .context(context.path().to_path_buf())
            .run("true")
            .build(make)
            .await
            .unwrap_err();
        assert!(
            format!("{refused:#}").contains("host's own filesystem"),
            "the server's refusal was swallowed: {refused:#}"
        );
    }

    /// The whole mapping, in one build: the `env` prefix on a `RUN`, `cp -a` for a `COPY`,
    /// `cd` for a `WORKDIR`, and a `commit` last.
    #[tokio::test]
    async fn each_step_goes_on_the_wire_as_the_design_says() {
        let context = tempfile::tempdir().unwrap();
        std::fs::write(context.path().join("app.py"), b"x").unwrap();
        let workfs = context.path().to_path_buf();

        let (make, log) = two(
            vec![unknown_image()],
            vec![
                took_the_session(&workfs),
                ran(0), // RUN
                ran(0), // COPY
                ran(0), // WORKDIR
                committed("cortex.local/built@sha256:whatever"),
            ],
        );

        let built = Rootfs::from_image("alpine:3.20")
            .context(context.path().to_path_buf())
            .env("TZ", "UTC")
            .run("apk add jq")
            .copy("app.py", "/srv/app.py")
            .workdir("/srv")
            .build(make)
            .await
            .expect("building");

        assert_eq!(
            argvs(&log),
            [
                vec!["env", "TZ=UTC", "sh", "-c", "apk add jq"],
                // The destination's parent is made first, and both paths travel as arguments
                // to the script rather than inside it.
                vec![
                    "sh",
                    "-c",
                    "mkdir -p -- \"$(dirname -- \"$2\")\" && cp -a -- \"$1\" \"$2\"",
                    "cp",
                    workfs.join("app.py").to_str().unwrap(),
                    "/srv/app.py"
                ],
                vec!["cd", "/srv"],
            ]
        );
        assert_eq!(built.env(), ["TZ=UTC"]);
        assert_eq!(built.working_dir(), Some("/srv"));
    }

    /// A build session says what it is at `init`: committable, with the context as its
    /// workfs, on the base the caller named, and delegating nothing — a build does not use
    /// delegation.
    #[tokio::test]
    async fn a_build_session_declares_itself() {
        let context = tempfile::tempdir().unwrap();
        let (make, log) = two(
            vec![unknown_image()],
            vec![
                took_the_session(context.path()),
                ran(0),
                committed("cortex.local/built@sha256:whatever"),
            ],
        );

        Rootfs::from_image("alpine:3.20")
            .context(context.path().to_path_buf())
            .run("true")
            .build(make)
            .await
            .expect("building");

        let calls = log.lock().unwrap();
        let Call::Init(init) = &calls[1] else {
            panic!("the build session did not begin with an init");
        };
        assert!(init.committable, "a build session cannot commit");
        assert!(init.delegated.is_empty(), "a build delegated something");
        assert_eq!(init.image.as_ref().unwrap().reference, "alpine:3.20");
        assert!(init.workfs.is_some(), "the context was not mounted");
    }

    /// The commit says the id the recipe computed, and what the image should state.
    #[tokio::test]
    async fn the_commit_says_the_id_and_what_the_image_states() {
        let context = tempfile::tempdir().unwrap();
        let (make, log) = two(
            vec![unknown_image()],
            vec![
                took_the_session(context.path()),
                ran(0),
                committed("cortex.local/built@sha256:whatever"),
            ],
        );

        let rootfs = Rootfs::from_image("alpine")
            .context(context.path().to_path_buf())
            .env("TZ", "UTC")
            .workdir("/srv");
        let expected = rootfs.id().unwrap();
        rootfs.build(make).await.expect("building");

        let calls = log.lock().unwrap();
        let Call::Commit(commit) = calls.last().unwrap() else {
            panic!("the last call was not a commit");
        };
        assert_eq!(commit.id, expected.to_string());
        assert_eq!(commit.env, ["TZ=UTC"]);
        assert_eq!(commit.working_dir.as_deref(), Some("/srv"));
    }

    /// A step that fails stops the build, and nothing is committed — so nothing is cached
    /// and the next attempt starts over.
    #[tokio::test]
    async fn a_failed_step_stops_the_build_and_commits_nothing() {
        let context = tempfile::tempdir().unwrap();
        let (make, log) = two(
            vec![unknown_image()],
            vec![took_the_session(context.path()), ran(3)],
        );

        let refused = Rootfs::from_image("alpine")
            .context(context.path().to_path_buf())
            .run("false")
            .run("never reached")
            .build(make)
            .await
            .unwrap_err();

        let failed = refused
            .downcast_ref::<StepFailed>()
            .expect("a step failure the caller can inspect");
        assert_eq!(failed.result.code, 3);
        assert_eq!(failed.step, Step::Run("false".into()));
        assert!(
            failed.argv.iter().any(|a| a == "false"),
            "the argv is not reported: {:?}",
            failed.argv
        );

        assert_eq!(argvs(&log).len(), 1, "the build carried on past a failure");
        assert!(
            !log.lock()
                .unwrap()
                .iter()
                .any(|call| matches!(call, Call::Commit(_))),
            "a failed build committed"
        );
    }

    /// `on_step` sees every step that reached the server, and only those — an `ENV` sends
    /// nothing and so has nothing to report.
    #[tokio::test]
    async fn on_step_sees_the_steps_that_ran() {
        let context = tempfile::tempdir().unwrap();
        let (make, _) = two(
            vec![unknown_image()],
            vec![
                took_the_session(context.path()),
                ran(0),
                ran(0),
                committed("cortex.local/built@sha256:whatever"),
            ],
        );

        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        Rootfs::from_image("alpine")
            .context(context.path().to_path_buf())
            .env("TZ", "UTC")
            .run("true")
            .workdir("/srv")
            .on_step(move |step, _| recorded.lock().unwrap().push(step.to_string()))
            .build(make)
            .await
            .expect("building");

        assert_eq!(*seen.lock().unwrap(), ["RUN true", "WORKDIR /srv"]);
    }

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
