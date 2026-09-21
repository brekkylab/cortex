//! The public end: a console server to run commands in, and the one channel it takes.
//!
//! # One channel, one direction of asking
//!
//! A console is one conversation, and only one end of it ever asks. This end sends a
//! request and the server answers it; there is no request the server issues, so this end
//! has no pending table, no listener, and no task waiting on something it also has to
//! read.
//!
//! What that leaves is a [`Console`] whose methods are round trips: one call out, one
//! answer back, and the answer is to the call that was just made. [`exec`](Console::exec)
//! is the largest of them and is still exactly that.
//!
//! # Waiting is the point, and a console does none of it in a thread
//!
//! Everything here is something to `await`: a command runs for as long as it runs, and a
//! backend with a kernel to bring up spends a cold start before the first one does. A
//! caller with several consoles wants all of them going at once, which is what an async
//! API gives for the price of a task each — where a thread each would have been a thread
//! parked on a pipe.
//!
//! What is *not* concurrent is one console. Its methods take `&mut self`, because the
//! protocol is one outstanding request at a time — see [`Client`] for why an exclusive
//! borrow is the honest way to say that.
//!
//! # Ending one
//!
//! Dropping it, and nothing else. A server exists for as long as a client does, so the
//! `quit` that lets it exit gracefully is owed exactly once and at exactly one moment —
//! when the console goes away. That is a lifetime, not a decision, so it is a
//! [`Drop`](Console#impl-Drop-for-Console) and not a method somebody has to remember.
//!
//! [`start`](Console::start) and [`stop`](Console::stop) are neither an ending nor
//! something to remember. They are **how a caller manages what the far end is holding**,
//! and that is the whole of what they do: a `stop` hands back the guest, the socket and
//! the scratch directory a booted session occupies while nothing is running, and a
//! `start` pays the cold start early so the first command does not pay it in its own
//! latency. Nothing is unlocked by either — the next command boots what it needs — so a
//! console that sends neither works, and one that sends both is one that knows when it
//! will be busy and when it will be idle.
//!
//! Neither is awaited for an answer, because there is nothing a caller would do
//! differently if one failed.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use futures_core::future::BoxFuture;
use tokio::process::Command;

use crate::{
    console::{
        base::{Client, Failure},
        message::{
            Call, CommitCall, Error, ExecCall, ExecResp, ImageSource, InitCall, NetworkAccess,
            Notification, ReadCall, ReadResp, Response, SecretAccess, TreeMount, TreeRole,
            TreeSource, WriteCall, WriteResp,
        },
        stdio::StdioClient,
    },
    fs::Mount,
    rootfs::Rootfs,
};

/// Whatever it takes to have a channel, deferred until there is a console to hold one.
///
/// [`Send`], because [`build`](ConsoleBuilder::build) awaits the `init` it sends and so
/// holds this across an await — which is what makes a builder something a task can be
/// spawned around, like everything else here. It costs nothing: a [`Client`] is already
/// `Send`, and so is what either setter captures.
type ClientFactory = Box<dyn FnOnce() -> anyhow::Result<Box<dyn Client>> + Send>;

/// Assembles a [`Console`] from the parts it needs.
///
/// A builder rather than arguments because a console is going to acquire more of them —
/// limits, a backend of its own choosing — and each should be something a caller can leave
/// out, as [`context`](Self::context) already is.
///
/// Nothing here starts anything. The channel is described, not driven, until something
/// is asked over it.
#[derive(Default)]
pub struct ConsoleBuilder {
    /// Not a client but the making of one, because some clients are a process away:
    /// [`stdio_client`](Self::stdio_client) is handed a program and not a channel, and
    /// starting a program can fail. Deferring that to [`build`](Self::build) keeps every
    /// setter infallible and leaves one place where a console either exists or says what
    /// it lacked.
    client_factory: Option<ClientFactory>,

    /// `None` is a console with nothing mounted, and stays `None`: a mount is something a
    /// caller *has* or has not, and there is no empty one to substitute. Such a session's
    /// commands see whatever the server's own filesystem holds, and this protocol has
    /// described none of it.
    context: Option<Box<dyn Mount>>,

    /// Where the session leaves what it produces, and `None` for a session whose output
    /// nobody collects. Held on the same terms as [`context`](Self::context) — a tree the
    /// caller mounted, kept alive for as long as the session is.
    artifacts: Option<Box<dyn Mount>>,

    /// Room for the session to work in, and `None` for a session that works wherever it
    /// can write.
    scratch: Option<Box<dyn Mount>>,

    /// The base the session's commands run in, and `None` to leave it to the server.
    image: Option<ImageSource>,

    /// `None` leaves the reach to the server, which is what a caller with no opinion wants —
    /// and what every caller wanted before this existed.
    network: Option<NetworkAccess>,

    /// Credentials the session's requests carry without its commands holding them, and empty to
    /// leave it to the server's own setting.
    secrets: Vec<SecretAccess>,

    /// A rootfs to build and then run in, and `None` for a session on an image that
    /// already exists somewhere.
    ///
    /// Mutually exclusive with [`image`](Self::image): both name what the session's commands
    /// run in, and a caller that said each of them meant one of them.
    rootfs: Option<Rootfs>,

    /// Whether this session may keep what it writes. False is what every caller wanted
    /// before this existed.
    committable: bool,
}

impl ConsoleBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drive the server over `client`.
    ///
    /// Anything that asks will do — a [`StdioClient`](crate::console::stdio::StdioClient)
    /// over a server it started, a virtio port into a guest, both ends in one process for
    /// a test. Whatever it took to have a channel is the client's, including a process if
    /// that is what it runs over, so there is nothing else here about where a server is.
    pub fn client(mut self, client: impl Client + 'static) -> Self {
        self.client_factory = Some(Box::new(move || Ok(Box::new(client))));
        self
    }

    /// Drive a server this console starts itself: `cmd`, over its own pipes.
    ///
    /// `cmd` is a program and its arguments — `["cortex-local-console"]`,
    /// `["sh", "-c", "…"]` — and that is the whole of what this shape of caller decides,
    /// since the two descriptors the protocol runs on are the client's. A caller who
    /// wants more of the command than that — an environment, a directory, somewhere for
    /// its stderr to go — builds the [`Command`] itself and hands it to
    /// [`StdioClient::new`], then the client to [`client`](Self::client).
    ///
    /// Starting it is [`build`](Self::build)'s, not this method's — nothing here starts
    /// anything, and a program that cannot be started, like a `cmd` with no program in
    /// it, is one of the ways building a console fails.
    pub fn stdio_client(mut self, cmd: &[impl AsRef<str>]) -> Self {
        // Owned before the closure, because the closure outlives this borrow and what it
        // was given has to still be there when `build` runs it.
        let cmd: Vec<String> = cmd.iter().map(|s| s.as_ref().to_string()).collect();

        self.client_factory = Some(Box::new(move || {
            let (program, args) = cmd
                .split_first()
                .context("a console server needs a program to run")?;

            let mut server = Command::new(program);
            server.args(args);

            let client = StdioClient::new(server).context("starting the console server")?;
            Ok(Box::new(client))
        }));
        self
    }

    /// Where this console's tree is mounted, which is what the session's context is.
    ///
    /// One per console and fixed for the session, because it is what every path in the
    /// session is spelled against: a `read` names a file under it, and so does the command
    /// that opens the same file by the same name.
    ///
    /// Mounting is the caller's, not the console's. Which binding puts a tree in front of a
    /// kernel is a build's business, and this is the whole of what the server is then told
    /// about it: [`build`](Self::build) names this mount point as the session's context —
    /// `file://` and the path — and the server answers where *it* plugged that in. The two
    /// need not be the same path, and on a server that runs commands in a kernel of its own
    /// they are not: what comes back is a path in there. Which is why the answer is read
    /// rather than assumed; see [`TreeMount`](crate::console::TreeMount).
    ///
    /// Takes the mount by value, so a console holds it and the tree is there for at least as
    /// long as the session is (see [`Mount`]). A caller that needs it elsewhere as well hands
    /// over an `Arc<..>` of it, which is a [`Mount`] too: how long a mount is held is the
    /// lifetime of the value, and whose lifetime that is, is the caller's to decide rather
    /// than something this end arranges behind it.
    ///
    /// A plain [`PathBuf`] is a [`Mount`], which is what a caller hands over for a directory
    /// the host already has: there is nothing to put up for one, and nothing this end takes
    /// down at the end of the session.
    ///
    /// **The session reads this tree and does not write in it.** A [`write`](Console::write)
    /// naming a path inside it is refused, and a server with a kernel of its own mounts it
    /// read-only so that a command cannot write there either — which is what
    /// [`artifacts`](Self::artifacts) and [`scratch`](Self::scratch) are for, and how a
    /// caller gets a tree back unchanged rather than a promise that it was not touched.
    ///
    /// Leaving it out is a console with nothing mounted; see the field this fills.
    pub fn context(mut self, mount: impl Mount + 'static) -> Self {
        self.context = Some(Box::new(mount));
        self
    }

    /// Where this session leaves what it produces.
    ///
    /// A second tree, on the same terms as [`context`](Self::context): the caller mounts it, the
    /// console names it at `init`, and the server answers where it put it — which is what
    /// [`artifacts_path`](Console::artifacts_path) then spells.
    ///
    /// **What it buys is knowing which files are the result.** A session that writes its
    /// output into the tree it was given leaves the caller to work out what is new, and that
    /// is a diff against a directory somebody else may also be writing to. A session that
    /// writes it here leaves the caller a tree whose whole contents are the answer.
    ///
    /// Leaving it out is a session whose output nobody collects, which is every session
    /// written before this existed.
    pub fn artifacts(mut self, mount: impl Mount + 'static) -> Self {
        self.artifacts = Some(Box::new(mount));
        self
    }

    /// Room for this session to work in, and where it starts.
    ///
    /// The third tree, and the one meant to be thrown away: a command that unpacks, builds,
    /// or writes something it will read back needs somewhere to put it that is neither
    /// somebody's project nor the output the caller will collect.
    ///
    /// **A session that has one starts in it** — the server answers this mount point as the
    /// session's `cwd` — so every relative path a command writes lands here without any
    /// command having to be told. That is the whole of what makes it worth passing rather
    /// than letting commands find `/tmp`: the caller decides what backs it and when it goes
    /// away.
    pub fn scratch(mut self, mount: impl Mount + 'static) -> Self {
        self.scratch = Some(Box::new(mount));
        self
    }

    /// The base the session's commands run in, named as an OCI image.
    ///
    /// ```no_run
    /// # use cortex::console::Console;
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = Console::builder()
    ///     .stdio_client(&["cortex-uvm-console"])
    ///     .image("python:3.13-slim")
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// **What a command finds around it is the image's**, which is what makes this worth
    /// saying at all: its interpreter, its libraries, and the environment it declares. A
    /// backend that has no base to swap answers
    /// [`UNSUPPORTED_IMAGE`](crate::console::Error::UNSUPPORTED_IMAGE) from
    /// [`build`](Self::build), which is every backend whose commands run on the server's own
    /// filesystem.
    ///
    /// A reference this server can parse but not fetch is **not** refused here. Pulling is
    /// slow enough to belong to a boot, so a name that resolves to nothing is heard from the
    /// first call that needs a guest, or from [`Console::start`] if one is made.
    ///
    /// Leaving it out leaves the choice to the server, and [`Console::image`] is then how to
    /// find out what it chose.
    pub fn image(mut self, image: impl Into<ImageSource>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// How much of a network the session's commands get.
    ///
    /// ```no_run
    /// # use cortex::console::{Console, NetworkAccess};
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = Console::builder()
    ///     .stdio_client(&["cortex-uvm-console"])
    ///     .network(NetworkAccess::public())
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// **A server gives what is named here or refuses to open the session**, which is what
    /// makes this worth saying rather than checking afterwards: a console that exists is one
    /// whose commands reach what was asked for and no more. A reach the far end cannot provide
    /// arrives as [`UNSUPPORTED_NETWORK`](crate::console::Error::UNSUPPORTED_NETWORK) from
    /// [`build`](Self::build) — including from a server whose commands run on this host, which
    /// cannot take the network away from them and so answers only
    /// [`full`](NetworkAccess::full).
    ///
    /// Leaving it out leaves the choice to the server, and [`Console::network`] is then how to
    /// find out what it chose.
    pub fn network(mut self, network: NetworkAccess) -> Self {
        self.network = Some(network);
        self
    }

    /// Add a credential the session's requests carry without its commands ever holding it.
    ///
    /// ```no_run
    /// # use cortex::console::{Console, NetworkAccess, SecretAccess};
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = Console::builder()
    ///     .stdio_client(&["cortex-uvm-console"])
    ///     .network(NetworkAccess::public())
    ///     .secret(SecretAccess::query("OPENWEATHER_API_KEY", "api.openweathermap.org"))
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// See [`SecretAccess`] for why only the variable name travels. A server that cannot intercept
    /// the request refuses a session that asks it to, with
    /// [`UNSUPPORTED_NETWORK`](crate::console::Error::UNSUPPORTED_NETWORK).
    pub fn secret(mut self, secret: SecretAccess) -> Self {
        self.secrets.push(secret);
        self
    }

    /// Add several credentials at once — [`secret`](Self::secret) for each.
    pub fn secrets(mut self, secrets: impl IntoIterator<Item = SecretAccess>) -> Self {
        self.secrets.extend(secrets);
        self
    }

    /// Build a rootfs, and run this session's commands in what it makes.
    ///
    /// ```no_run
    /// # use cortex::console::Console;
    /// # use cortex::rootfs::Rootfs;
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = Console::builder()
    ///     .stdio_client(&["cortex-uvm-console"])
    ///     .rootfs(Rootfs::from_image("alpine:3.20").run("apk add --no-cache jq"))
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    ///
    /// **A build that has already been done is not done again.** The recipe's digest names
    /// the image it makes, so the first thing [`build`](Self::build) sends is an `init` on
    /// that name — which either takes the session, and there was nothing to do, or is
    /// refused and the build runs. The common case costs exactly what a session on an
    /// existing image costs.
    ///
    /// **So this can take minutes.** A session on an image is a process and one message; a
    /// session on a rootfs that nobody has built yet is a boot, every step, and a commit.
    /// [`on_step`](Rootfs::on_step) is how to watch it happen.
    ///
    /// The build runs in a session of its own, with the recipe's context as its tree
    /// rather than whatever [`context`](Self::context) names — a recipe says what it copies from,
    /// and that is not the session's business.
    ///
    /// [`network`](Self::network) is the exception, and it reaches both: the build asks for
    /// the reach this session asked for, and for the internet when this session asked for
    /// nothing. A recipe carries no reach of its own, because what a build may touch is a
    /// property of the machine it runs on and not of the image it makes — two callers with
    /// the same recipe and different network policies still get the same image.
    ///
    /// Refused together with [`image`](Self::image): both say what the commands run in.
    pub fn rootfs(mut self, rootfs: Rootfs) -> Self {
        self.rootfs = Some(rootfs);
        self
    }

    /// Let this session [`commit`](Console::commit) what it writes.
    ///
    /// Asked for rather than always on, because it costs something on a backend that has to
    /// arrange for it: a micro-VM one keeps a root it would otherwise let go of, and is given
    /// a scratch directory to write the result into. A session that will never commit should
    /// not carry either.
    pub fn committable(mut self) -> Self {
        self.committable = true;
        self
    }

    /// Fails for the one part that has no default — something to ask — for whatever having
    /// a channel took (over stdio, a server process that would not start), and for the
    /// `init` this then sends.
    ///
    /// A runtime has to be under it: a console over [`stdio_client`](Self::stdio_client)
    /// starts a process, and a process is registered with the runtime that will reap it.
    /// Building one from outside a task or `main` is a panic, not an `Err` — the missing
    /// runtime is the caller's own shape and not something the channel could report.
    ///
    /// Usually one message. The exception is [`rootfs`](Self::rootfs), whose image this may
    /// have to make before there is a session to be had — see there for what that costs.
    pub async fn build(self) -> anyhow::Result<Console> {
        Console::new(self).await
    }
}

/// Open the build's own session on `client`, run the steps, and keep what they wrote.
///
/// A second `init` on the same channel, which the protocol says replaces the first — so a
/// build costs no second server and no second process. The session it makes is the build's:
/// committable, on the recipe's base, with the recipe's context as its tree. The one the
/// caller asked for is opened afterwards, by the `init` that follows this.
async fn build_it(
    client: &mut dyn Client,
    rootfs: &mut Rootfs,
    plan: &crate::rootfs::Plan,
) -> anyhow::Result<()> {
    let answered = client
        .init(InitCall {
            context: Some(plan.context.clone()),
            // Neither belongs to a build. What a build produces is an image and not a tree of
            // files somebody collects, and where its steps write is the root it is building —
            // which is the whole of what a `commit` then keeps. Handing it either would be
            // two places a step could write and one of them thrown away.
            artifacts: None,
            scratch: None,
            image: Some(plan.base.clone()),
            // The internet, whatever the session asked for. A build's first step is
            // almost always a fetch — the server's own default is the host and no
            // further, which would fail nearly every recipe at a refused connection —
            // and, unlike the session, this is not something the caller opened. A caller
            // asks for a *session*; how the image that session runs on gets made is
            // cortex's, and it is made the same way whatever the session may reach.
            //
            // The reach ends with the build. The session's own `init` follows this one
            // and carries [`ConsoleBuilder::network`], so a recipe that fetched what the
            // image is made of does not leave a session that can fetch anything.
            //
            // The thing this gives up: a caller cannot ask for a build that reaches
            // nothing, and a caller who asked the session for more than `public` — a
            // [`full`](NetworkAccess::full) reach, or host ports for a mirror on this
            // machine — does not get it here. Neither has come up; both are a knob on
            // the recipe when one does.
            network: Some(NetworkAccess::public()),
            // A build fetches packages and holds no credential of the session's; the secrets a
            // session was configured with are for the commands it runs, not for making its image.
            secrets: Vec::new(),
            committable: true,
        })
        .await?;

    let at = answered
        .context
        .map(|at| at.path)
        .context("the console server took the build context and did not say where it put it")?;

    crate::rootfs::run(client, rootfs, plan, Path::new(&at)).await
}

/// A console: something to run commands in.
///
/// An [`exec`](Self::exec) per command, and dropping it to end the session. That is the
/// whole of what a caller has to do: what the session *is* was said when the console was
/// built, booting happens under the first command that needs it, and whatever it took
/// goes away with the console.
///
/// [`start`](Self::start) and [`stop`](Self::stop) are how a caller manages what the far
/// end is holding — pay the cold start early, hand the resources back while idle — and
/// neither is required: a console that sends neither runs the same commands to the same
/// results.
///
/// ```no_run
/// use cortex::console::Console;
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// // Whichever console server this is: the client starts it and owns it from here.
/// // Building also says what the session is — the tree it works in if there is one — so
/// // a console that exists is one the server has answered. Nothing is booted by that;
/// // the command below pays for the boot, unless a `start` gets there first.
/// let mut console = Console::builder()
///     .stdio_client(&["cortex-local-console"])
///     .build()
///     .await?;
///
/// // `None`: this command has no opinion about how long it may take, so the console's
/// // default stands. `Some(ms)` is how one says otherwise.
/// let result = console.exec(["sh", "-c", "echo hi"], None).await?;
/// assert_eq!(result.stdout, b"hi\n");
///
/// // And the session ends when the console goes — here, at the end of the scope. The
/// // server hears `quit` and exits rather than being killed, releasing what it booted
/// // on the way out, so no `stop` is owed.
/// # Ok(())
/// # }
/// ```
/// One of a session's trees: the mount this end holds, and where the server put it.
///
/// **The two are one value because they are one fact.** A tree this console named is a tree
/// the server answered a path for — a console whose context went nowhere does not exist, see
/// [`placed`] — so a mount with no path and a path with no mount are both states this end
/// would have to decide what to do about, and neither can happen. Holding them apart was
/// two `Option`s that had to agree, checked by a match; holding them together is the check.
struct Tree {
    /// **Never read, and that is the whole job.** A mount is only promised to be there for
    /// as long as the value is (see [`Mount`]), so holding one here is what keeps the tree
    /// up for as long as the session is: a console that let go of a mount it had put up
    /// would go on naming paths at a directory the kernel no longer answers for. What the
    /// protocol is *spelled* in is [`path`](Self::path), which is the server's answer and
    /// not this.
    ///
    /// Whether this is the last holder is the caller's business — what arrives here is
    /// whatever was passed to the builder, an `Arc<..>` included.
    #[allow(dead_code)]
    mount: Box<dyn Mount>,

    /// Where the server put it, as it answered at `init`.
    ///
    /// **The paths this protocol speaks are these.** A [`read`](Console::read) names a file
    /// under one, and so does a [`write`](Console::write). It is the server's answer and not
    /// the mount point above: a host-local server answers the mount point itself, and one
    /// with a guest answers where the tree is *in the guest*, which is not a path this host
    /// has at all.
    ///
    /// Where the session *stands* is not here and is not kept anywhere on this side: the
    /// current directory is the far end's state machine, `init` says where it starts, and
    /// after that a caller asks with `pwd` like anyone at a terminal. A copy here would be
    /// a second answer to a question that already has one — wrong from the first `cd`.
    path: PathBuf,
}

pub struct Console {
    /// A console has one, always. What ending needs is not for this to become absent but
    /// for it to be *replaced* — see [`Console::drop`](Console#impl-Drop-for-Console).
    client: Box<dyn Client>,

    /// The tree this session works in — `None` when this console has nothing mounted.
    context: Option<Tree>,

    /// Where this session leaves what it produces — `None` when the caller named none.
    artifacts: Option<Tree>,

    /// Room for this session to work in, and where it starts — `None` when the caller
    /// named none.
    scratch: Option<Tree>,

    /// The base in force, as the server answered at `init`.
    image: Option<ImageSource>,

    /// What the session's commands can reach, as the server answered at `init` — `None` from a
    /// server that would not say.
    network: Option<NetworkAccess>,
}

impl Console {
    pub fn builder() -> ConsoleBuilder {
        ConsoleBuilder::default()
    }

    /// Take a channel and announce the session on it.
    ///
    /// `init` is here rather than a method a caller remembers, because a session's shape
    /// is not something a console is ever without: the tree, the base and the reach are
    /// what the builder was given, they outlive every execution, and there is no useful
    /// console in between having a channel and having said what is on it. So a `Console`
    /// that exists is one the server has heard from and answered — which is the one thing
    /// about a session a caller can act on before asking for work.
    ///
    /// The answer is not only an acknowledgement: it says where the server put the context,
    /// and that path is what every later `read` and `write` is spelled in.
    /// A server that takes a context and says nothing about where it went has left this end
    /// with no way to name a file, so that is a console that does not exist rather than one
    /// that guesses.
    ///
    /// Nothing is booted or mounted by it. The first command that needs a session boots
    /// one, unless a [`start`](Self::start) gets there first.
    ///
    /// A failure here takes the channel with it. The client is dropped rather than told
    /// `quit`, because there is no `Console` to owe one: over stdio that kills the server
    /// process instead of asking it to leave, which is the same ending a console dropped
    /// off a runtime gets.
    pub async fn new(builder: ConsoleBuilder) -> anyhow::Result<Self> {
        let ConsoleBuilder {
            client_factory,
            context: context_mount,
            artifacts: artifacts_mount,
            scratch: scratch_mount,
            image,
            network,
            secrets,
            rootfs,
            committable,
        } = builder;

        // Before the channel, so a caller that said both hears it without a process having
        // been started for a session that could not be described.
        anyhow::ensure!(
            !(image.is_some() && rootfs.is_some()),
            "a session names both an image and a rootfs, and both say what its commands run \
             in — name one"
        );

        let client_factory =
            client_factory.context("a console needs a client to drive its server")?;
        let mut client = client_factory()?;

        // What the server is told about each tree is where this end has it. A mount point
        // with no URL to it is refused here rather than sent, by the rule the client factory
        // above follows: a console either exists or says what it lacked.
        let context = named(TreeRole::Context, context_mount.as_deref())?;
        let artifacts = named(TreeRole::Artifacts, artifacts_mount.as_deref())?;
        let scratch = named(TreeRole::Scratch, scratch_mount.as_deref())?;

        // Worked out before anything is sent, and on a blocking thread because it reads
        // every file a `COPY` names. Its whole point is that the `init` below can *be* the
        // cache probe: a build whose image is already here costs one message.
        let mut rootfs = rootfs;
        let plan = match rootfs.as_ref() {
            None => None,
            Some(declared) => {
                // The two halves, cloned out: the declaration and where its `COPY`s read
                // from. Not the `Rootfs`, which also holds the caller's `on_step` and so
                // cannot cross to a thread of its own.
                let recipe = declared.recipe().clone();
                let context = declared.context_dir().to_path_buf();
                Some(
                    tokio::task::spawn_blocking(move || crate::rootfs::plan(&recipe, &context))
                        .await
                        .context("working out what this build is called")??,
                )
            }
        };

        let session = InitCall {
            context: context.clone(),
            artifacts: artifacts.clone(),
            scratch: scratch.clone(),
            image: plan.as_ref().map(|plan| plan.image.clone()).or(image),
            network: network.clone(),
            secrets,
            committable,
        };

        let mut answered = client.init(session.clone()).await;

        // Refused because nobody has built it yet — which is not a failure, it is the other
        // half of asking. Build it, then ask again.
        if let (Err(Failure::Refused(refusal)), Some(plan), Some(rootfs)) =
            (&answered, &plan, rootfs.as_mut())
            && refusal.code == Error::UNKNOWN_IMAGE
        {
            build_it(&mut *client, rootfs, plan).await?;
            answered = client.init(session).await;
        }
        let answered = answered?;

        Ok(Console {
            client,
            context: placed(TreeRole::Context, context_mount, answered.context)?,
            artifacts: placed(TreeRole::Artifacts, artifacts_mount, answered.artifacts)?,
            scratch: placed(TreeRole::Scratch, scratch_mount, answered.scratch)?,
            image: answered.image,
            network: answered.network,
        })
    }

    /// Where the server put this session's tree, and so what every path this console sends
    /// is relative to — `None` when nothing is mounted.
    ///
    /// A caller builds a [`read`](Self::read) or a [`write`](Self::write) path by joining
    /// onto this, and reaches the same files on this host by joining onto the mount point it
    /// handed over instead. The two are not required to be the same directory and on a
    /// server with a guest they are not — which is why the protocol settles which one it
    /// speaks in rather than leaving it assumed.
    pub fn context_path(&self) -> Option<&Path> {
        self.context.as_ref().map(|tree| tree.path.as_path())
    }

    /// Where the server put this session's [`artifacts`](ConsoleBuilder::artifacts) tree —
    /// `None` when the caller named none.
    ///
    /// What the session leaves here is what the caller asked for, so this is the path to
    /// join a [`read`](Self::read) onto to collect it — and the mount this end holds is the
    /// other way to reach the same files, since a mounted tree is a directory on this host.
    pub fn artifacts_path(&self) -> Option<&Path> {
        self.artifacts.as_ref().map(|tree| tree.path.as_path())
    }

    /// Where the server put this session's [`scratch`](ConsoleBuilder::scratch) tree —
    /// `None` when the caller named none.
    ///
    /// Also where the session stands to begin with, which is the reason to pass one. That is
    /// only the *beginning*: a command can move the session and nothing reports that it did,
    /// so a caller that needs to know where it stands now runs `pwd` — see
    /// [`exec`](Self::exec).
    pub fn scratch_path(&self) -> Option<&Path> {
        self.scratch.as_ref().map(|tree| tree.path.as_path())
    }

    /// Whether this session has a tree at all, which is whether a path this console sends
    /// can name one of the caller's files.
    ///
    /// The same answer as [`context_path`](Self::context_path) being `Some`, for a caller
    /// that wants the question and not the path: a [`read`](Self::read) or a
    /// [`write`](Self::write) has nowhere to join onto without one, so this is what to ask
    /// before building a path rather than after failing to.
    pub fn has_context(&self) -> bool {
        self.context.is_some()
    }

    /// Whether this session has somewhere to leave what the caller came for.
    ///
    /// A session without one still runs commands; what it does not have is a tree whose
    /// contents outlive it on the caller's terms. Worth asking before running the work that
    /// would write there — see [`artifacts_path`](Self::artifacts_path) for where it goes.
    pub fn has_artifacts(&self) -> bool {
        self.artifacts.is_some()
    }

    /// Whether this session has a tree to work in, and so whether it stands in one.
    ///
    /// Without it the session starts wherever the server puts a session with nothing
    /// mounted, and writes that go nowhere named go nowhere the caller can reach — see
    /// [`scratch_path`](Self::scratch_path).
    pub fn has_scratch(&self) -> bool {
        self.scratch.is_some()
    }

    /// The base this session's commands run in, as the server answered.
    ///
    /// The [`ConsoleBuilder::image`] that was asked for, in the server's own spelling: a
    /// reference naming no registry names a default one, and this is where a client sees which.
    /// Worth reading when nothing was asked, since the base is then the server's own choice.
    ///
    /// `None` is a base with no reference to give — a server that will not say, or one whose
    /// commands do not run in an image at all.
    pub fn image(&self) -> Option<&ImageSource> {
        self.image.as_ref()
    }

    /// What this session's commands can reach, as the server answered.
    ///
    /// The reach [`ConsoleBuilder::network`] asked for, when it asked — a server gives that or
    /// refuses, so a console that exists is one that got it. Worth reading when nothing was
    /// asked: the reach is then the server's own choice, and this is where it says which.
    ///
    /// `None` is a server that would not say, which is every server built before it could.
    pub fn network(&self) -> Option<&NetworkAccess> {
        self.network.as_ref()
    }

    /// Boot the far end now, to hide the cold start.
    ///
    /// Entirely optional, and it unlocks nothing: an [`exec`](Self::exec),
    /// [`read`](Self::read) or [`write`](Self::write) that reaches a stopped session is
    /// served by a server that boots one first. **What it buys is who waits.** Booting is
    /// the expensive part on a backend with a kernel to bring up, and a caller that sends
    /// this as soon as it has a console pays for it in parallel with whatever it does
    /// next — deciding what to run, waiting on a model, reading a file — instead of
    /// inside the latency of its first command.
    ///
    /// So it is worth sending exactly when a caller knows a console will be used and does
    /// not yet know what for, which is most of them.
    ///
    /// `Ok` is the message having gone out and not the server having booted: nothing
    /// answers it. A boot that fails is heard by whoever asks for the next thing that
    /// needed one, as [`BOOT_FAILED`](crate::console::Error::BOOT_FAILED).
    pub async fn start(&mut self) -> Result<(), Failure> {
        self.client.start().await
    }

    /// Release what booting took, to stop occupying it while nothing is running.
    ///
    /// Not the end of anything, and not owed: the next call that needs a booted session
    /// gets one, and dropping the console releases everything anyway. **What it buys is
    /// what a booted session is not holding in the meantime** — a guest's memory, a
    /// socket, a scratch directory — which is worth a message when a caller knows it is
    /// going idle, and worth nothing between two commands a second apart.
    ///
    /// The other half of [`start`](Self::start)'s trade, and the cost is the same one:
    /// the next command pays a cold start again. A caller that will be idle for minutes
    /// takes that gladly; one that will be idle for a moment should not.
    ///
    /// `Ok` is the message having gone out. Like [`start`](Self::start), nothing answers
    /// it.
    pub async fn stop(&mut self) -> Result<(), Failure> {
        self.client.stop().await
    }

    /// Run one command, and return everything it produced.
    ///
    /// The command is an argv — `["echo", "hi"]` — and nothing here consults a shell, so
    /// a caller that wants shell semantics asks for them outright: `["sh", "-c", ".."]`.
    ///
    /// Where it runs is [where the session stands](Self::context_path), which is the far
    /// end's to keep: this carries no directory, because a second answer to that question
    /// would disagree with the first the moment a command ran `cd`. A caller that wants one
    /// command somewhere else says so in the command — `sh -c 'cd there && ..'`.
    ///
    /// `timeout_ms` bounds this execution, and `None` leaves it to run until it finishes
    /// — or forever, if it is the kind of command that does not finish.
    ///
    /// Which is the only way to bound one, and the reason the parameter is here rather
    /// than something a caller arranges outside. Dropping this future leaves the server
    /// running a command nobody is waiting for and a channel with an answer still to come
    /// on it — so a caller that wants to stop waiting says so to the server, which is the
    /// end that can also stop the command.
    pub async fn exec(
        &mut self,
        cmd: impl IntoIterator<Item = impl AsRef<str>>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecResp, Failure> {
        let exec = ExecCall {
            cmd: cmd.into_iter().map(|s| s.as_ref().to_string()).collect(),
            timeout_ms,
        };

        self.client.exec(exec).await
    }

    /// Read part of a file where commands run.
    ///
    /// The path is the server's — under [`context_path`](Self::context_path), which is what a
    /// caller joins onto — so this names the file a command would open by the same name, and
    /// is how a caller sees what an execution wrote to a file rather than to its output.
    ///
    /// `offset` is where to start, `None` being the beginning; `len` is how much to ask
    /// for, `None` being the rest. Neither is a promise about what comes back — a file
    /// too large for one message arrives in pieces, and [`ReadResp::size`] against what
    /// did arrive is the only thing that says there are more.
    pub async fn read(
        &mut self,
        path: impl AsRef<str>,
        offset: Option<u64>,
        len: Option<u64>,
    ) -> Result<ReadResp, Failure> {
        let read = ReadCall {
            path: path.as_ref().to_string(),
            offset,
            len,
        };
        self.client.read(read).await
    }

    /// Put bytes in a file where commands run, and hear how big it is afterwards.
    ///
    /// The other direction of [`read`](Self::read), and the way to put something where
    /// a command will find it. The path is the server's, the same way.
    ///
    /// `offset` is the difference between replacing a file and writing into one. `None`
    /// makes the file *be* `data` — created if it was not there, cut to length if it was
    /// — where `Some(0)` writes the same bytes at the same place and leaves whatever lay
    /// past them. A caller that means to replace a file sends `None`.
    pub async fn write(
        &mut self,
        path: impl AsRef<str>,
        data: impl Into<Vec<u8>>,
        offset: Option<u64>,
    ) -> Result<WriteResp, Failure> {
        let write = WriteCall {
            path: path.as_ref().to_string(),
            data: data.into(),
            offset,
        };
        self.client.write(write).await
    }

    /// Keep what this session has written, under `id`, and hear what image it made.
    ///
    /// `env` and `working_dir` are what the image should say about running a process in it.
    /// They are stated rather than observed: a session's own environment holds things that
    /// belong to the session, and only the caller knows which of them it meant to keep.
    ///
    /// Refused unless the console was built [`committable`](ConsoleBuilder::committable), and
    /// refused outright by a backend with nothing to keep — one whose commands run on the
    /// server's own filesystem has no base to have written over.
    ///
    /// What comes back is namable: hand it to [`ConsoleBuilder::image`] and the next session
    /// starts where this one left off.
    pub async fn commit(
        &mut self,
        id: impl Into<String>,
        env: Vec<String>,
        working_dir: Option<String>,
    ) -> Result<ImageSource, Failure> {
        let commit = CommitCall {
            id: id.into(),
            env,
            working_dir,
        };
        // The one member a client is entitled to. It is `Option` on the wire because the end
        // that writes the layer has no image to name — see `CommitResp` — but that end is
        // never the one a client is talking to, so a response without it here is a backend
        // that answered wrongly rather than a case to hand back.
        self.client.commit(commit).await?.image.ok_or_else(|| {
            Failure::Refused(Error {
                code: Error::INTERNAL_ERROR,
                message: "the server kept this session's layer and did not name the image \
                          it made"
                    .into(),
                data: None,
            })
        })
    }
}

impl Drop for Console {
    /// Say `quit`, which is what lets the server exit rather than be killed.
    ///
    /// A server exists for as long as the client driving it does, so this is owed exactly
    /// once and this is the moment: nothing else can happen on the channel afterwards.
    /// Only `quit` — a server that hears it releases whatever a
    /// [`stop`](Console::stop) would have released, on its way out — so an ending is one
    /// message and not two.
    ///
    /// Nobody hears what it answered. There is no caller left to tell by the time a
    /// console is being dropped, and over stdio what `quit` reports is the server
    /// process's exit status, which nothing could act on anyway.
    ///
    /// # Why the client is swapped rather than taken
    ///
    /// Saying `quit` is an `await` and a `drop` cannot wait for one, so it goes to a task
    /// — which needs the client to own. A field cannot be *moved* out of a type that has
    /// a destructor, because the destructor would then run on whatever was left behind
    /// and there is no `Box` that means "nothing".
    ///
    /// It can be *swapped*, though, and the value swapped in is right here: a client that
    /// answers nothing, which is exactly what a console whose session is over has. So the
    /// field above stays an unconditional `Box` — a console has a client, always — where
    /// an `Option` would have put a state that cannot happen into every method that asks.
    ///
    /// Off a runtime, or on one that shuts down before the task is polled, the client is
    /// dropped without the message. That is still an ending, just an abrupt one: over
    /// stdio the server process is killed rather than asked, and what it left behind is
    /// swept by the next run.
    fn drop(&mut self) {
        /// Answers nothing, because by the time this is reachable there is nothing left
        /// to answer with.
        struct Spent;

        impl Client for Spent {
            fn call(&mut self, _: Call) -> BoxFuture<'_, Result<Response, Failure>> {
                Box::pin(async { Err(Failure::broken("the session has ended")) })
            }

            fn notify(&mut self, _: Notification) -> BoxFuture<'_, Result<(), Failure>> {
                Box::pin(async { Err(Failure::broken("the session has ended")) })
            }
        }

        // Zero-sized, so this `Box` is a dangling pointer rather than an allocation.
        let mut client = std::mem::replace(&mut self.client, Box::new(Spent));

        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = client.quit().await;
            });
        }
    }
}

/// A mount this end holds, as the URL the far end is told to realize — or `None` for a tree
/// the caller named none of.
///
/// `role` is the member it will travel under, and is here only so that a caller who mounted
/// something unnameable is told *which* of its trees that was — the protocol's own name for
/// it, so that this end and the server call it the same thing in a failure.
fn named(role: TreeRole, mount: Option<&dyn Mount>) -> anyhow::Result<Option<TreeSource>> {
    let Some(mount) = mount else {
        return Ok(None);
    };
    let url = mount.url().with_context(|| {
        format!(
            "a mount point that is not an absolute UTF-8 path cannot be named as a {role}: {:?}",
            mount.mountpoint()
        )
    })?;
    Ok(Some(TreeSource::new(url)))
}

/// The tree this end holds and the path the server answered for it, paired — or `None` for a
/// tree the caller named none of.
///
/// **A tree the server took and did not place is not a session.** Every path the caller would
/// send for it afterwards would be a guess, so this is a console that does not exist rather
/// than one that carries a mount it cannot name a file in. A path that is not absolute is
/// refused for the same reason one step further on: it would be relative to a working
/// directory the server never named.
///
/// A path answered for a tree that was never asked for is dropped. It describes a session
/// this end did not name, and there is nothing here it could be a path to.
fn placed(
    role: TreeRole,
    mount: Option<Box<dyn Mount>>,
    at: Option<TreeMount>,
) -> anyhow::Result<Option<Tree>> {
    let Some(mount) = mount else {
        return Ok(None);
    };
    let at = at.with_context(|| {
        format!(
            "the console server took the {role} at {} and did not say where it put it",
            mount.mountpoint().display()
        )
    })?;

    let path = PathBuf::from(at.path);
    anyhow::ensure!(
        path.is_absolute(),
        "the console server put the {role} somewhere that is not an absolute path: {}",
        path.display()
    );
    Ok(Some(Tree { mount, path }))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_core::future::BoxFuture;

    use super::*;
    use crate::console::message::{
        Call, Error, InitResp, Method, Notification, Response, TreeMount,
    };

    /// What a [`Recorder`] was handed, readable while it is still lent out.
    ///
    /// Two lists because they answer different questions: `methods` is what went out and
    /// in what order, including the `quit`, which is a notification and so never a `Call`;
    /// `calls` is what those carried.
    #[derive(Clone)]
    struct Log {
        methods: Arc<Mutex<Vec<Method>>>,
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl Log {
        fn methods(&self) -> Vec<Method> {
            self.methods.lock().unwrap().clone()
        }

        /// The `n`th call, counted over calls alone.
        fn call(&self, n: usize) -> Call {
            self.calls.lock().unwrap()[n].clone()
        }
    }

    /// A client over canned answers, recording everything it was handed.
    struct Recorder {
        answers: Vec<Response>,
        log: Log,
    }

    impl Client for Recorder {
        fn call(&mut self, call: Call) -> BoxFuture<'_, Result<Response, Failure>> {
            self.log.methods.lock().unwrap().push(call.method());
            self.log.calls.lock().unwrap().push(call);
            let answer = if self.answers.is_empty() {
                // Not a panic: an ending stops a console best-effort, and a test that has
                // said all it means to say should not have to answer that too.
                Err(Failure::broken("nothing left to answer with"))
            } else {
                match self.answers.remove(0) {
                    Response::Error(error) => Err(Failure::Refused(error)),
                    answer => Ok(answer),
                }
            };
            Box::pin(async move { answer })
        }

        fn notify(&mut self, notification: Notification) -> BoxFuture<'_, Result<(), Failure>> {
            self.log.methods.lock().unwrap().push(notification.method());
            Box::pin(async { Ok(()) })
        }
    }

    fn recorder(answers: Vec<Response>) -> (Recorder, Log) {
        let log = Log {
            methods: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        (
            Recorder {
                answers,
                log: log.clone(),
            },
            log,
        )
    }

    /// A session taken with nothing mounted, which is what a server answers a context-less
    /// `init` with.
    fn initialized() -> Response {
        Response::Init(InitResp::default())
    }

    /// A session taken, with the context put at `path`.
    fn initialized_at(path: &Path) -> Response {
        Response::Init(InitResp {
            context: Some(TreeMount {
                path: path.to_str().expect("a test path is UTF-8").to_string(),
            }),
            ..InitResp::default()
        })
    }

    /// An execution that ran and ended.
    fn ran(stdout: &[u8]) -> Response {
        Response::Exec(ExecResp {
            code: 0,
            stdout: stdout.to_vec(),
            ..ExecResp::default()
        })
    }

    /// A builder needs exactly one thing, and says which when it does not have it.
    #[tokio::test]
    async fn a_console_needs_something_to_ask() {
        let Err(failure) = Console::builder().build().await else {
            panic!("a console with nothing to ask should not build");
        };
        assert!(failure.to_string().contains("needs a client"), "{failure}");
    }

    /// Every way building can fail says which one it was, and all of them are here rather
    /// than spread over the first few methods a caller would have reached for.
    #[tokio::test]
    async fn a_stdio_console_starts_its_server_when_it_is_built() {
        let Err(e) = Console::builder()
            .stdio_client(&["cortex-no-such-console"])
            .build()
            .await
        else {
            panic!("a console over a program that does not exist should not build");
        };
        assert!(e.to_string().contains("starting the console server"), "{e}");

        // A command with nothing to run is the same kind of failure and reported the same
        // way: at build, saying what it lacked.
        let Err(e) = Console::builder()
            .stdio_client(&[] as &[&str])
            .build()
            .await
        else {
            panic!("a console over no program at all should not build");
        };
        assert!(e.to_string().contains("needs a program to run"), "{e}");

        // And a program that starts but does not speak the protocol fails here too, which
        // is what moved when `init` became the builder's: it is answered or the console
        // does not exist. `cat` echoes the request back, so what arrives is a request and
        // not the response that was due — arguments are the caller's, and go to the
        // program as given.
        let Err(e) = Console::builder()
            .stdio_client(&["cat", "-u"])
            .build()
            .await
        else {
            panic!("a console over a program that cannot answer should not build");
        };
        assert!(e.to_string().contains("cannot answer"), "{e}");
    }

    /// Nothing about the session is kept on this side once `init` has gone out: what a
    /// caller asks for goes out as asked, in that order, and the answer is the server's.
    ///
    /// Which is also what makes an execution one round trip. There is no second message
    /// underneath a command, so the methods below are exactly what the caller asked for.
    #[tokio::test]
    async fn a_session_is_the_servers_to_keep() {
        let (client, log) = recorder(vec![initialized(), ran(b"hi\n")]);
        let mut console = Console::builder().client(client).build().await.unwrap();

        // Optional, and unanswered: only the timing of the boot below changes.
        console.start().await.unwrap();
        assert_eq!(
            console.exec(["echo", "hi"], None).await.unwrap().stdout,
            b"hi\n"
        );
        // A `stop` in the middle: what booting took is handed back and the session stays
        // open, which is the only thing `stop` is for.
        console.stop().await.unwrap();

        // And ending is the console going away — one `quit`, and nothing else. What the
        // server releases on hearing it is the server's business, so no second `stop`
        // goes out to ask for it.
        drop(console);
        tokio::task::yield_now().await;

        assert_eq!(
            log.methods(),
            [
                Method::Init,
                Method::Start,
                Method::Exec,
                Method::Stop,
                Method::Quit
            ]
        );

        // `init` is the first thing on the channel, before anything a caller asked for,
        // and a console with nothing mounted describes a session by saying nothing.
        let Call::Init(init) = log.call(0) else {
            panic!("{:?} is not an init", log.call(0));
        };
        assert_eq!(init, InitCall::default());

        // The command went out as the caller wrote it, carrying nothing else: where it
        // runs is the far end's, and this end has nothing to add.
        let Call::Exec(exec) = log.call(1) else {
            panic!("{:?} is not an exec", log.call(1));
        };
        assert_eq!(
            exec,
            ExecCall {
                cmd: vec!["echo".into(), "hi".into()],
                timeout_ms: None,
            }
        );
    }

    /// A session the server refuses to have is not a console, so there is nothing for a
    /// caller to hold and nothing to end.
    #[tokio::test]
    async fn a_console_the_server_will_not_have_does_not_exist() {
        let (client, log) = recorder(vec![Response::Error(Error::new(
            Error::UNSUPPORTED_CONTEXT,
            "s3: this server realizes file:// and nothing else",
        ))]);
        let Err(e) = Console::builder().client(client).build().await else {
            panic!("a console whose init was refused should not build");
        };
        assert!(e.to_string().contains("file://"), "{e}");

        // And no `quit`: what would owe one is a `Console`, and there is not one.
        tokio::task::yield_now().await;
        assert_eq!(log.methods(), [Method::Init]);
    }

    /// A console that was never started is ended the same way, because what `quit` is for
    /// is the server going away and a server exists whether or not it was ever booted.
    #[tokio::test]
    async fn dropping_a_console_ends_its_session() {
        let (client, log) = recorder(vec![initialized()]);
        let console = Console::builder().client(client).build().await.unwrap();

        drop(console);

        // The ending is somebody else's turn to run, so wait for one.
        tokio::task::yield_now().await;
        assert_eq!(log.methods(), [Method::Init, Method::Quit]);
    }

    /// The mount this end holds is what `init` names, and where it went is the server's
    /// answer — which is the path every later call in the session is spelled in, and not
    /// the mount point that was sent.
    ///
    /// The two need not be the same directory — a server with a guest of its own answers a
    /// path in there — so they are different here, to make visible which of them a caller
    /// joins onto: what came back.
    #[tokio::test]
    async fn a_console_names_its_mount_and_reads_back_where_it_went() {
        let (client, log) = recorder(vec![initialized_at(Path::new("/srv/served"))]);
        let console = Console::builder()
            .client(client)
            .context(PathBuf::from("/mnt/here"))
            .build()
            .await
            .unwrap();

        let Call::Init(init) = log.call(0) else {
            panic!("{:?} is not an init", log.call(0));
        };
        assert_eq!(init.context, Some(TreeSource::new("file:///mnt/here")));
        assert_eq!(console.context_path(), Some(Path::new("/srv/served")));
    }

    /// A session with a context needs somewhere to have put it: a path is what every later
    /// call is spelled in, so a server that answers without one has left this end with
    /// nothing it could name, and that is not a console.
    #[tokio::test]
    async fn a_context_the_server_did_not_place_is_not_a_session() {
        let (client, _) = recorder(vec![initialized()]);
        let Err(e) = Console::builder()
            .client(client)
            .context(PathBuf::from("/mnt/here"))
            .build()
            .await
        else {
            panic!("a console whose context went nowhere should not build");
        };
        assert!(e.to_string().contains("did not say where"), "{e}");

        // And a path that is not one this end could join onto is the same kind of nothing.
        let (client, _) = recorder(vec![initialized_at(Path::new("relative/served"))]);
        let Err(e) = Console::builder()
            .client(client)
            .context(PathBuf::from("/mnt/here"))
            .build()
            .await
        else {
            panic!("a console whose context went to a relative path should not build");
        };
        assert!(e.to_string().contains("not an absolute path"), "{e}");
    }
}
