//! The builder: what a caller says a build is, and what running it means.
//!
//! [`Rootfs`] is declaration and nothing else. It has no terminal method: nothing here
//! contacts a server, nothing is read except what [`id`](Rootfs::id) has to hash, and the
//! value can be carried around and asked its id before anything is started. A caller hands
//! it to [`ConsoleBuilder::rootfs`](crate::console::ConsoleBuilder::rootfs), and the console
//! that comes back is a session in what it describes — built on the way there if nobody has
//! built it yet.
//!
//! It is two things beside each other: a [`Recipe`](super::Recipe), which is what the build
//! declares and can be written down, and a context directory, which is where this machine
//! keeps what a `COPY` reads. Everything that computes takes them as a pair, because the
//! line between them is the line between what travels and what does not.
//!
//! The rest of this module is what a build *means*: [`plan`] works out its name from the
//! declaration alone, and [`run`] turns the steps into `exec`s on a session someone else
//! opened.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::console::{Client, CommitCall, ExecCall, ExecResp, ImageSource, WorkFsSource};

use super::{BuildId, Recipe, Step, digest};

/// A build: a base, the steps over it, and what those steps are allowed to see.
///
/// Built by chaining — every setter takes and returns `self` — because a build is one
/// declaration and a half-configured one is not a thing worth having a name for.
///
/// What it does *not* hold is a console, and it has no method that takes one. A `Rootfs` is
/// handed to [`ConsoleBuilder::rootfs`](crate::console::ConsoleBuilder::rootfs), which is
/// where a server is spoken to — so this value is inert, and a caller can build one, hash it,
/// store it, and decide later whether anything runs.
pub struct Rootfs {
    /// What this build declares — the base and the steps — and nothing about this machine.
    /// The half that can be written down and read back somewhere else.
    recipe: Recipe,
    /// What a `COPY` source is relative to, and what is mounted as the build session's
    /// workfs. Defaults to the current directory, which is what a caller writing
    /// `.copy("app", …)` means by `app`.
    ///
    /// Beside the recipe rather than in it: an absolute path on one machine means nothing on
    /// another, so a recipe carrying one could not travel.
    context: PathBuf,
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
type Observer = Box<dyn FnMut(&Step, &ExecResp) + Send>;

/// Something a Dockerfile said that this build did not act on.
///
/// Handed back by [`Rootfs::from_dockerfile`] rather than printed. A library that only
/// prints is one whose behaviour cannot be asserted, and one that prints *as well* has
/// decided on the caller's behalf where its diagnostics go.
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
        Rootfs::from_recipe(Recipe::new(reference, Vec::new()))
    }

    /// A build over a recipe that already exists — one read back from wherever it was
    /// stored, or built up as a [`Recipe`] rather than by chaining.
    ///
    /// The [`context`](Self::context) starts at the current directory, as it does for
    /// [`from_image`](Self::from_image), because a recipe carries none: that is what lets it
    /// be the same recipe on another machine. A recipe with a `COPY` in it needs one named
    /// here.
    pub fn from_recipe(recipe: Recipe) -> Self {
        Rootfs {
            recipe,
            context: PathBuf::from("."),
            on_step: None,
        }
    }

    /// A command, run through `sh -c` with everything [`env`](Self::env) has said so far.
    pub fn run(mut self, command: impl Into<String>) -> Self {
        self.recipe.steps.push(Step::Run(command.into()));
        self
    }

    /// Something from the build context, copied into the image.
    ///
    /// `src` is relative to the [`context`](Self::context) directory; an absolute one is
    /// refused when the build runs, because it names a place the context does not contain
    /// and so is not part of what this build declared.
    pub fn copy(mut self, src: impl AsRef<Path>, dst: impl Into<String>) -> Self {
        self.recipe.steps.push(Step::Copy {
            src: src.as_ref().to_path_buf(),
            dst: dst.into(),
        });
        self
    }

    /// A variable, for every later [`run`](Self::run) and for what the image states.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.recipe.steps.push(Step::Env {
            key: key.into(),
            value: value.into(),
        });
        self
    }

    /// Where later steps run, and what the image states.
    pub fn workdir(mut self, dir: impl Into<String>) -> Self {
        self.recipe.steps.push(Step::Workdir(dir.into()));
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

    /// Watch each step as it finishes.
    ///
    /// Called for every step that reached the server, which is every step but
    /// [`env`](Self::env) — that one accumulates here and sends nothing. It observes and
    /// cannot change anything: the return value is dropped, and a panic in it takes the
    /// build down with it.
    pub fn on_step(mut self, f: impl FnMut(&Step, &ExecResp) + Send + 'static) -> Self {
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
        digest(&self.recipe, &self.context)
    }

    /// What this build declares: the base, and the steps in order.
    ///
    /// The serializable half — see [`Recipe`] for what storing one does and does not carry.
    pub fn recipe(&self) -> &Recipe {
        &self.recipe
    }

    /// The directory a `COPY` reads from, as the caller named it.
    ///
    /// Relative until a build resolves it, which is why this is the path that was given
    /// rather than the one that will be mounted.
    pub fn context_dir(&self) -> &Path {
        &self.context
    }
}

/// What the built image should state about running a process in it: the accumulated
/// environment as `KEY=VALUE`, and the last working directory.
///
/// Computed from the steps rather than observed from a session, so the answer is the same
/// whether the build ran or was found in the cache.
pub(crate) fn stated(steps: &[Step]) -> (Vec<String>, Option<String>) {
    let mut env: Vec<(&str, &str)> = Vec::new();
    let mut working_dir = None;
    for step in steps {
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

/// What a console has to know before it sends anything: what the build is called, and what
/// it will have to say to make it.
///
/// Worked out from the declaration alone — no server is contacted — which is what lets the
/// first `init` a console sends *be* the cache probe rather than a round trip in front of
/// one.
pub(crate) struct Plan {
    /// What the built image is called, which is the digest of the recipe.
    pub id: BuildId,
    /// That, as a reference a server understands.
    pub image: ImageSource,
    /// The base to build over, as the caller spelled it.
    pub base: ImageSource,
    /// The build context, as the workfs a build session works in.
    pub workfs: WorkFsSource,
    /// What the built image should state, accumulated from the steps.
    pub env: Vec<String>,
    pub working_dir: Option<String>,
}

/// Work out [`Plan`] from a declaration.
///
/// Takes the two halves of a [`Rootfs`] rather than the `Rootfs` itself, because a `Rootfs`
/// carries the caller's [`on_step`](Rootfs::on_step) — a closure, which cannot be handed to
/// a thread of its own, and which this has no use for anyway.
///
/// **Blocking**, because computing the id reads every file a `COPY` names — a caller on a
/// runtime owes this a thread of its own.
pub(crate) fn plan(recipe: &Recipe, context: &Path) -> anyhow::Result<Plan> {
    let id = digest(recipe, context)?;

    // Absolute, because a workfs is named to the server as a `file://` URL and a relative
    // path after `file://` reads as a host. Resolved here rather than in `context` so that a
    // caller can name a directory that does not exist yet and be told when it is used.
    let context = std::path::absolute(context)
        .with_context(|| format!("resolving the build context at {}", context.display()))?;
    let context = context.to_str().with_context(|| {
        format!(
            "a build context that is not UTF-8 cannot be named as a workfs: {:?}",
            context
        )
    })?;

    let (env, working_dir) = stated(&recipe.steps);
    Ok(Plan {
        image: ImageSource::new(reference(&id)),
        id,
        base: ImageSource::new(recipe.base.clone()),
        workfs: WorkFsSource::new(format!("file://{context}")),
        env,
        working_dir,
    })
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
    pub result: ExecResp,
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

/// Run the steps on a session that is already up, and keep what they wrote.
///
/// The session `client` holds must be the build one — committable, on the base, with the
/// context mounted — and `at` is where the server said it put that context. Everything about
/// arranging that is [`ConsoleBuilder::build`](crate::console::ConsoleBuilder::build)'s,
/// because it is the end that holds the client; what is here is the part that has to know
/// what a step *means*.
pub(crate) async fn run(
    client: &mut dyn Client,
    rootfs: &mut Rootfs,
    plan: &Plan,
    at: &Path,
) -> anyhow::Result<()> {
    let mut running: Vec<(String, String)> = Vec::new();

    for step in &rootfs.recipe.steps {
        let argv = match step {
            Step::Env { key, value } => {
                match running.iter_mut().find(|(k, _)| k == key) {
                    Some(existing) => existing.1 = value.clone(),
                    None => running.push((key.clone(), value.clone())),
                }
                // Nothing on the wire: there is no session-level environment, so this rides
                // on every later command instead.
                continue;
            }
            Step::Run(command) => {
                // `env` in front rather than a shell assignment, because the argv is already
                // split and no shell is consulted to build it — so no value needs quoting
                // and a value with a space in it cannot become two words.
                let mut argv = vec!["env".to_string()];
                argv.extend(running.iter().map(|(k, v)| format!("{k}={v}")));
                argv.push("sh".into());
                // `-c` and not `-lc`. A login shell sources `/etc/profile`, and what
                // `/etc/profile` does on every base worth naming is *assign* `PATH` — which
                // throws away the one the agent built, `/abin` with it. A build would then
                // be unable to run the executables cortex provides it.
                argv.push("-c".into());
                argv.push(command.clone());
                argv
            }
            Step::Copy { src, dst } => {
                inside_context(src)?;
                let from = at.join(src);
                let from = from.to_str().with_context(|| {
                    format!("{} is not a UTF-8 path to name to a server", from.display())
                })?;
                // The destination's parent is made first, because `COPY app /srv/app` with
                // no `/srv` is the ordinary Dockerfile spelling — `docker build` creates the
                // path and a bare `cp -a` does not, which would fail an instruction this
                // adapter had already accepted.
                //
                // One `sh -c` rather than two steps, so a caller watching `on_step` sees the
                // one instruction it declared.
                vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "mkdir -p -- \"$(dirname -- \"$2\")\" && cp -a -- \"$1\" \"$2\"".to_string(),
                    // `sh -c … name arg1 arg2`: the paths travel as arguments rather than
                    // inside the script, so nothing in either is ever word-split or read as
                    // a shell operator.
                    "cp".to_string(),
                    from.to_string(),
                    dst.clone(),
                ]
            }
            // `cd` is a protocol builtin and moves the session, so it persists across the
            // steps after it rather than ending with the command.
            Step::Workdir(dir) => vec!["cd".to_string(), dir.clone()],
        };

        let result = client
            .exec(ExecCall {
                cmd: argv.clone(),
                timeout_ms: None,
            })
            .await?;
        if let Some(on_step) = rootfs.on_step.as_mut() {
            on_step(step, &result);
        }
        if result.code != 0 {
            // Nothing is committed, so nothing is cached and the next attempt starts over.
            return Err(anyhow::Error::new(StepFailed {
                step: step.clone(),
                argv,
                result,
            }));
        }
    }

    client
        .commit(CommitCall {
            id: plan.id.to_string(),
            env: plan.env.clone(),
            working_dir: plan.working_dir.clone(),
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use crate::BoxFuture;
    use crate::console::{
        Call, Client, CommitResp, Console, ConsoleBuilder, Error, Failure, ImageSource, InitCall,
        InitResp, Notification, Response, WorkFsMount,
    };

    /// Every call a build made, in order.
    type Log = Arc<Mutex<Vec<Call>>>;

    /// A client over canned answers, recording every call it was handed.
    struct Recorder {
        answers: Vec<Response>,
        log: Log,
    }

    impl Client for Recorder {
        fn call(&mut self, call: Call) -> BoxFuture<'_, Result<Response, Failure>> {
            self.log.lock().unwrap().push(call);
            let answer = match self.answers.is_empty() {
                true => Err(Failure::Refused(Error {
                    code: Error::INTERNAL_ERROR,
                    message: "the test ran out of answers".into(),
                    data: None,
                })),
                // A refusal becomes a `Refused` here, which is what a transport does with
                // one — a caller never meets `Response::Error` itself.
                false => match self.answers.remove(0) {
                    Response::Error(error) => Err(Failure::Refused(error)),
                    answer => Ok(answer),
                },
            };
            Box::pin(async move { answer })
        }

        fn notify(&mut self, _: Notification) -> BoxFuture<'_, Result<(), Failure>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// A console builder over canned answers, and the log of everything sent to it.
    ///
    /// One client for the whole thing, because that is what a build is here: the session's
    /// own `init`, and — only if that one is refused — a build session on the same channel
    /// and then the session's `init` again.
    fn answering(answers: Vec<Response>) -> (ConsoleBuilder, Log) {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let builder = ConsoleBuilder::new().client(Recorder {
            answers,
            log: log.clone(),
        });
        (builder, log)
    }

    /// A session taken, with the workfs put where the build said it was.
    fn took_the_session(at: &Path) -> Response {
        Response::Init(InitResp {
            workfs: Some(WorkFsMount {
                path: at.to_str().unwrap().to_string(),
            }),
            ..InitResp::default()
        })
    }

    fn ran(code: i32) -> Response {
        Response::Exec(ExecResp {
            code,
            ..ExecResp::default()
        })
    }

    fn committed(reference: &str) -> Response {
        Response::Commit(CommitResp {
            size: 4096,
            image: Some(ImageSource::new(reference)),
        })
    }

    /// The argv of every `exec` in a log, in order.
    fn argvs(log: &Log) -> Vec<Vec<String>> {
        log.lock()
            .unwrap()
            .iter()
            .filter_map(|call| match call {
                Call::Exec(exec) => Some(exec.cmd.clone()),
                _ => None,
            })
            .collect()
    }

    /// The `init` calls in a log, in order.
    fn inits(log: &Log) -> Vec<InitCall> {
        log.lock()
            .unwrap()
            .iter()
            .filter_map(|call| match call {
                Call::Init(init) => Some(init.clone()),
                _ => None,
            })
            .collect()
    }

    /// What a build refused with.
    ///
    /// Not `unwrap_err`, which wants `Debug` on the other side — and a `Console` holds a
    /// live channel, which is not a thing to print.
    fn refusal(built: anyhow::Result<Console>) -> anyhow::Error {
        match built {
            Ok(_) => panic!("this was expected to fail, and a session came back"),
            Err(refused) => refused,
        }
    }

    /// The one answer that means the image is not there and the build should run.
    fn unknown_image() -> Response {
        Response::Error(Error {
            code: Error::UNKNOWN_IMAGE,
            message: "no such built image".into(),
            data: None,
        })
    }

    /// An image that already exists is not built again. The whole session costs one `init` —
    /// no boot, no steps, no commit.
    #[tokio::test]
    async fn an_image_that_exists_is_not_built_again() {
        let context = tempfile::tempdir().unwrap();
        let rootfs = Rootfs::from_image("alpine")
            .context(context.path().to_path_buf())
            .env("TZ", "UTC")
            .run("true");
        let id = rootfs.id().unwrap();
        let reference = format!("cortex.local/built@{id}");

        let (builder, log) = answering(vec![Response::Init(InitResp {
            image: Some(ImageSource::new(&reference)),
            ..InitResp::default()
        })]);
        let console = builder.rootfs(rootfs).build().await.expect("a session");

        assert_eq!(console.image().unwrap().reference, reference);
        assert!(argvs(&log).is_empty(), "a cache hit ran a step");
        assert_eq!(
            log.lock().unwrap().len(),
            1,
            "a cache hit sent more than the session's own init"
        );
    }

    /// The session's own `init` names the built image, and is otherwise the session the
    /// caller described — a rootfs says what the commands run *in*, and nothing else.
    #[tokio::test]
    async fn the_first_init_names_the_built_image_and_nothing_more() {
        let context = tempfile::tempdir().unwrap();
        let rootfs = Rootfs::from_image("alpine")
            .context(context.path().to_path_buf())
            .run("true");
        let id = rootfs.id().unwrap();

        let (builder, log) = answering(vec![Response::Init(InitResp::default())]);
        builder.rootfs(rootfs).build().await.expect("a session");

        let init = inits(&log).remove(0);
        assert_eq!(
            init.image.as_ref().unwrap().reference,
            format!("cortex.local/built@{id}")
        );
        // Neither asked for by this caller, and neither borrowed from the build: a build is
        // committable and mounts its context, and this session is not and does not.
        assert!(!init.committable, "the build's commit right leaked");
        assert!(init.workfs.is_none(), "the build's context leaked");
    }

    /// An image nobody has built is built, and then the session is asked for again.
    /// `UNKNOWN_IMAGE` is the answer that says so, and it is the only one that means
    /// "carry on".
    #[tokio::test]
    async fn an_image_nobody_has_built_is_built() {
        let context = tempfile::tempdir().unwrap();
        let (builder, log) = answering(vec![
            unknown_image(),
            took_the_session(context.path()),
            ran(0),
            committed("cortex.local/built@sha256:whatever"),
            Response::Init(InitResp::default()),
        ]);

        builder
            .rootfs(
                Rootfs::from_image("alpine")
                    .context(context.path().to_path_buf())
                    .run("true"),
            )
            .build()
            .await
            .expect("a session on what the build made");

        assert_eq!(argvs(&log), [vec!["env", "sh", "-c", "true"]]);
        // Three: the one that was refused, the build's, and the one that was answered.
        let inits = inits(&log);
        assert_eq!(inits.len(), 3);
        assert_eq!(
            inits[0], inits[2],
            "the session asked for after the build is not the one asked for before it"
        );
    }

    /// Any other refusal is the caller's to hear. A server that cannot swap a base at all
    /// answers `UNSUPPORTED_IMAGE`, and building anyway would just fail again more slowly.
    #[tokio::test]
    async fn a_refusal_that_is_not_about_the_cache_is_reported() {
        let context = tempfile::tempdir().unwrap();
        let (builder, log) = answering(vec![Response::Error(Error {
            code: Error::UNSUPPORTED_IMAGE,
            message: "commands here run on this host's own filesystem".into(),
            data: None,
        })]);

        let refused = refusal(
            builder
                .rootfs(
                    Rootfs::from_image("alpine")
                        .context(context.path().to_path_buf())
                        .run("true"),
                )
                .build()
                .await,
        );
        assert!(
            format!("{refused:#}").contains("host's own filesystem"),
            "the server's refusal was swallowed: {refused:#}"
        );
        assert_eq!(
            inits(&log).len(),
            1,
            "a refusal that is not the cache built"
        );
    }

    /// The whole mapping, in one build: the `env` prefix on a `RUN`, `cp -a` for a `COPY`,
    /// `cd` for a `WORKDIR`, and a `commit` last.
    #[tokio::test]
    async fn each_step_goes_on_the_wire_as_the_design_says() {
        let context = tempfile::tempdir().unwrap();
        std::fs::write(context.path().join("app.py"), b"x").unwrap();
        let workfs = context.path().to_path_buf();

        let (builder, log) = answering(vec![
            unknown_image(),
            took_the_session(&workfs),
            ran(0), // RUN
            ran(0), // COPY
            ran(0), // WORKDIR
            committed("cortex.local/built@sha256:whatever"),
            Response::Init(InitResp::default()),
        ]);

        builder
            .rootfs(
                Rootfs::from_image("alpine:3.20")
                    .context(context.path().to_path_buf())
                    .env("TZ", "UTC")
                    .run("apk add jq")
                    .copy("app.py", "/srv/app.py")
                    .workdir("/srv"),
            )
            .build()
            .await
            .expect("a session on what the build made");

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
    }

    /// A build session says what it is at `init`: committable, with the context as its
    /// workfs, on the base the caller named rather than the image being built, and reaching
    /// the internet because this session asked for nothing.
    #[tokio::test]
    async fn a_build_session_declares_itself() {
        let context = tempfile::tempdir().unwrap();
        let (builder, log) = answering(vec![
            unknown_image(),
            took_the_session(context.path()),
            ran(0),
            committed("cortex.local/built@sha256:whatever"),
            Response::Init(InitResp::default()),
        ]);

        builder
            .rootfs(
                Rootfs::from_image("alpine:3.20")
                    .context(context.path().to_path_buf())
                    .run("true"),
            )
            .build()
            .await
            .expect("a session on what the build made");

        let init = inits(&log).remove(1);
        assert!(init.committable, "a build session cannot commit");
        assert_eq!(init.image.as_ref().unwrap().reference, "alpine:3.20");
        assert!(init.workfs.is_some(), "the context was not mounted");
        // Said without being asked for: a build's first step is usually a fetch, so a
        // server default of "the host and no further" would fail nearly every build.
        assert_eq!(
            init.network.as_ref().map(|reach| reach.reach.as_str()),
            Some("public"),
            "a build under a session that said nothing did not get the internet"
        );
    }

    /// The commit says the id the recipe computed, and what the image should state.
    #[tokio::test]
    async fn the_commit_says_the_id_and_what_the_image_states() {
        let context = tempfile::tempdir().unwrap();
        let (builder, log) = answering(vec![
            unknown_image(),
            took_the_session(context.path()),
            ran(0), // the WORKDIR; the ENV sends nothing
            committed("cortex.local/built@sha256:whatever"),
            Response::Init(InitResp::default()),
        ]);

        let rootfs = Rootfs::from_image("alpine")
            .context(context.path().to_path_buf())
            .env("TZ", "UTC")
            .workdir("/srv");
        let expected = rootfs.id().unwrap();
        builder
            .rootfs(rootfs)
            .build()
            .await
            .expect("a session on what the build made");

        let calls = log.lock().unwrap();
        let Some(Call::Commit(commit)) = calls.iter().find(|call| matches!(call, Call::Commit(_)))
        else {
            panic!("the build did not commit");
        };
        assert_eq!(commit.id, expected.to_string());
        assert_eq!(commit.env, ["TZ=UTC"]);
        assert_eq!(commit.working_dir.as_deref(), Some("/srv"));
    }

    /// A session that asked for a reach gets it, and so does the build under it. The
    /// internet is the default, not a floor: a caller who sandboxed the session meant the
    /// build too, and there is no second knob on the recipe to contradict it with.
    #[tokio::test]
    async fn a_build_reaches_what_its_session_asked_for() {
        let context = tempfile::tempdir().unwrap();
        let (builder, log) = answering(vec![
            unknown_image(),
            took_the_session(context.path()),
            ran(0),
            committed("cortex.local/built@sha256:whatever"),
            Response::Init(InitResp::default()),
        ]);

        builder
            .network(crate::console::NetworkAccess::none())
            .rootfs(
                Rootfs::from_image("alpine")
                    .context(context.path().to_path_buf())
                    .run("true"),
            )
            .build()
            .await
            .expect("a session on what the build made");

        let reaches: Vec<_> = inits(&log)
            .iter()
            .map(|init| {
                init.network
                    .as_ref()
                    .map(|reach| reach.reach.clone())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            reaches,
            ["none", "none", "none"],
            "the build did not get the reach its session asked for"
        );
    }

    /// A step that fails stops the build, and nothing is committed — so nothing is cached
    /// and the next attempt starts over.
    #[tokio::test]
    async fn a_failed_step_stops_the_build_and_commits_nothing() {
        let context = tempfile::tempdir().unwrap();
        let (builder, log) = answering(vec![
            unknown_image(),
            took_the_session(context.path()),
            ran(3),
        ]);

        let refused = refusal(
            builder
                .rootfs(
                    Rootfs::from_image("alpine")
                        .context(context.path().to_path_buf())
                        .run("false")
                        .run("never reached"),
                )
                .build()
                .await,
        );

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
        // And no session on top of it: there is no image for one to run in.
        assert_eq!(inits(&log).len(), 2);
    }

    /// `on_step` sees every step that reached the server, and only those — an `ENV` sends
    /// nothing and so has nothing to report.
    #[tokio::test]
    async fn on_step_sees_the_steps_that_ran() {
        let context = tempfile::tempdir().unwrap();
        let (builder, _) = answering(vec![
            unknown_image(),
            took_the_session(context.path()),
            ran(0),
            ran(0),
            committed("cortex.local/built@sha256:whatever"),
            Response::Init(InitResp::default()),
        ]);

        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        builder
            .rootfs(
                Rootfs::from_image("alpine")
                    .context(context.path().to_path_buf())
                    .env("TZ", "UTC")
                    .run("true")
                    .workdir("/srv")
                    .on_step(move |step, _| recorded.lock().unwrap().push(step.to_string())),
            )
            .build()
            .await
            .expect("a session on what the build made");

        assert_eq!(*seen.lock().unwrap(), ["RUN true", "WORKDIR /srv"]);
    }

    /// A session cannot be told twice what its commands run in. Refused before a process is
    /// started, because there is nothing to ask a server about.
    #[tokio::test]
    async fn a_session_names_an_image_or_a_rootfs_and_not_both() {
        let (builder, log) = answering(Vec::new());
        let refused = refusal(
            builder
                .image("alpine:3.20")
                .rootfs(Rootfs::from_image("alpine").run("true"))
                .build()
                .await,
        );
        assert!(
            format!("{refused:#}").contains("name one"),
            "the wrong complaint: {refused:#}"
        );
        assert!(log.lock().unwrap().is_empty(), "something was sent anyway");
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
            rootfs.recipe().steps,
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
        assert_eq!(rootfs.recipe().base, "alpine:3.20");
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
        let (env, working_dir) = stated(&rootfs.recipe().steps);
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
        let (env, _) = stated(&rootfs.recipe().steps);
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
