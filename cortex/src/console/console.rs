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
            Call, ExecCall, ExecResp, InitCall, MountSpec, NetworkAccess, Notification, ReadCall,
            ReadResp, Response, WriteCall, WriteResp,
        },
        stdio::StdioClient,
    },
    fs::Mount,
    image::Image,
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
/// out, as [`mount`](Self::mount) already is.
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

    /// The image a session's commands run in, and `None` to leave it to the server: a
    /// backend that runs commands on the host it is already on has nothing to boot, and one
    /// that boots a machine says so rather than guessing a base on the caller's behalf.
    image: Option<Image>,

    /// What a previous session wrote, for a console that does not start from scratch, and
    /// `None` for one that does. Currently an ext4 blob.
    snapshot: Option<Vec<u8>>,

    /// Every tree this console gives the session, in the order they will be mounted.
    ///
    /// Empty is a console with nothing mounted: such a session's commands see whatever the
    /// server's own filesystem holds, and this protocol has described none of it.
    mounts: Vec<Mounted>,

    /// `None` leaves the reach to the server, which is what a caller with no opinion wants —
    /// and what every caller wanted before this existed.
    network: Option<NetworkAccess>,
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

    /// Give the session a tree, mounted at `at`.
    ///
    /// `at` is where the tree appears to the session's commands, and it is this end's to
    /// choose: [`build`](Self::build) names it in the `init`, so every path the console will
    /// send is known before the session exists. A [`read`](Console::read) names a file under
    /// it, so does a [`write`](Console::write), and so does the command that opens the same
    /// file by the same name. It has to be absolute — a relative one would be relative to a
    /// working directory nobody named — and a console whose mount point cannot be spelled as
    /// a URL does not build.
    ///
    /// Mounting *this* end is the caller's, not the console's. Which binding puts a tree in
    /// front of a kernel is a build's business, and what the server is told about it is the
    /// URL and the path — see [`Mount::url`].
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
    /// **Called as many times as there are trees, and the order is kept.** A session that is
    /// given somebody's project and somewhere to leave its result names two, and a tree meant
    /// to sit inside another is named after it. What each tree is *for* is the caller's own
    /// and is nowhere in the protocol — what a tree is at a path, and whether a write may
    /// land in it, is the whole of what both ends agree on.
    ///
    /// Writable. [`mount_readonly`](Self::mount_readonly) is the other half of that choice.
    pub fn mount(mut self, mount: impl Mount + 'static, at: impl Into<PathBuf>) -> Self {
        self.mounts.push(Mounted {
            mount: Box::new(mount),
            at: at.into(),
            readonly: false,
        });
        self
    }

    /// Give the session a tree it may read and not write, at `at`.
    ///
    /// [`mount`](Self::mount) on every other count. **What it buys is getting the tree back
    /// unchanged rather than a promise that nothing touched it:** a [`write`](Console::write)
    /// naming a path under it is refused, and a server with a kernel of its own mounts it
    /// read-only so that a command cannot write there either.
    ///
    /// Which is what to hand somebody's project over as — and what makes a second, writable
    /// tree worth naming beside it, since a session that writes its output into the tree it
    /// was given leaves the caller to work out which files are new.
    pub fn mount_readonly(mut self, mount: impl Mount + 'static, at: impl Into<PathBuf>) -> Self {
        self.mounts.push(Mounted {
            mount: Box::new(mount),
            at: at.into(),
            readonly: true,
        });
        self
    }

    /// The base the session's commands run in.
    ///
    /// ```no_run
    /// # use cortex::console::Console;
    /// # use cortex::image::Image;
    /// # async fn f() -> anyhow::Result<()> {
    /// let console = Console::builder()
    ///     .stdio_client(&["cortex-uvm-console"])
    ///     .image(
    ///         Image::new()
    ///             .base("alpine:3.20")
    ///             .step("apk add --no-cache jq"),
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(()) }
    /// ```
    pub fn image(mut self, image: impl Into<Image>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// Start this session on what a previous one wrote, rather than from scratch.
    pub fn snapshot(mut self, snapshot: Vec<u8>) -> Self {
        self.snapshot = Some(snapshot);
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

    /// Fails for the one part that has no default — something to ask — for whatever having
    /// a channel took (over stdio, a server process that would not start), and for the
    /// `init` this then sends.
    ///
    /// A runtime has to be under it: a console over [`stdio_client`](Self::stdio_client)
    /// starts a process, and a process is registered with the runtime that will reap it.
    /// Building one from outside a task or `main` is a panic, not an `Err` — the missing
    /// runtime is the caller's own shape and not something the channel could report.
    ///
    /// Usually one message. The exception is [`image`](Self::image), which this may have to
    /// build before there is a session to be had — see there for what that costs.
    pub async fn build(self) -> anyhow::Result<Console> {
        Console::new(self).await
    }
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
/// One of a session's trees: the mount this end holds, and where the session sees it.
///
/// **The two are one value because they are one fact.** A tree this console holds up is a
/// tree it named a path for, so a mount with no path and a path with no mount are both
/// states this end would have to decide what to do about, and neither can happen.
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

    /// Where the session sees it, as `init` named it.
    ///
    /// **The paths this protocol speaks are these.** A [`read`](Console::read) names a file
    /// under one, and so does a [`write`](Console::write). It is not the mount point above:
    /// a host-local server may put the tree at the same place, and one with a guest puts it
    /// where the session can see it, which need not be a path this host has at all.
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

    /// Every tree this session was given, in the order they were named — empty when this
    /// console has nothing mounted.
    mounts: Vec<Tree>,
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
    /// Where each tree goes is settled here and not read back: a mount carries the path it
    /// appears at, so the paths every later `read` and `write` is spelled in are this end's
    /// own and are known before the `init` goes out.
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
            image,
            mounts,
            network,
            snapshot,
        } = builder;

        let client_factory =
            client_factory.context("a console needs a client to drive its server")?;
        let mut client = client_factory()?;

        // Every tree is turned into what the server is told about it before anything goes
        // out, by the rule the client factory above follows: a console either exists or says
        // what it lacked. A mount point with no URL to it and a guest path this protocol
        // cannot spell are both that, and both are this end's own mistake rather than
        // something to hear back from a server.
        let mut specs = Vec::with_capacity(mounts.len());
        let mut held = Vec::with_capacity(mounts.len());
        for mounted in mounts {
            specs.push(named(&mounted)?);
            held.push(Tree {
                mount: mounted.mount,
                path: mounted.at,
            });
        }

        let session = InitCall {
            mounts: specs,
            image,
            network: network.clone(),
            snapshot,
        };

        client.init(session).await?;

        Ok(Console {
            client,
            mounts: held,
        })
    }

    /// Where each of this session's trees appears, in the order they were named — what
    /// every path this console sends is relative to.
    ///
    /// A caller builds a [`read`](Self::read) or a [`write`](Self::write) path by joining
    /// onto one of these, and reaches the same files on this host by joining onto the mount
    /// point it handed over instead. The two are not required to be the same directory and
    /// on a server with a guest they are not — which is why what a path is spelled in is the
    /// guest path this end named rather than whatever the mount point happened to be.
    ///
    /// Empty is a console with nothing mounted: a `read` or a `write` has nowhere to join
    /// onto, so this is what to ask before building a path rather than after failing to.
    pub fn mounts(&self) -> impl ExactSizeIterator<Item = &Path> {
        self.mounts.iter().map(|tree| tree.path.as_path())
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
    /// Where it runs is where the session stands, which is the far
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
    /// The path is the session's — under one of [`mounts`](Self::mounts), which is what a
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

    /// Take everything this session has written, as a blob another session can start on.
    ///
    /// The other half of [`ConsoleBuilder::snapshot`]: what comes back is what that takes, so a
    /// session is carried on by building a new one with these bytes in hand. What is in them
    /// is the server's own encoding of the changes and not a caller's to read — keeping them
    /// and handing them back is the whole of the contract.
    ///
    /// Everything since the session began, and not since the last time this was asked: a
    /// snapshot is where a session *is*, so two taken in a row give the same thing twice and
    /// the second is not the difference between them.
    pub async fn snapshot(&mut self) -> Result<Vec<u8>, Failure> {
        self.client.snapshot().await.map(|answer| answer.blob)
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

/// A tree the caller handed over, as the builder holds it until there is a session to name
/// it in.
struct Mounted {
    mount: Box<dyn Mount>,
    at: PathBuf,
    readonly: bool,
}

/// A mount this end holds, as the one string the far end is told to realize.
///
/// Both halves can fail and both fail here: a mount point that cannot be spelled as a URL
/// has no name that travels, and a guest path this protocol cannot read back is one the
/// server would refuse. Neither is worth a round trip to find out, and neither leaves a
/// caller anything to do but fix the call.
fn named(mounted: &Mounted) -> anyhow::Result<MountSpec> {
    let url = mounted.mount.url().with_context(|| {
        format!(
            "a mount point that is not an absolute UTF-8 path cannot be named: {:?}",
            mounted.mount.mountpoint()
        )
    })?;

    let at = mounted.at.to_str().with_context(|| {
        format!(
            "a tree can only be mounted at a UTF-8 path: {:?}",
            mounted.at
        )
    })?;

    // The refusal already names the spec it was reading, which is both halves of what a
    // caller would have to be told to fix it.
    let spec = MountSpec::new(url, at)?;

    Ok(if mounted.readonly {
        spec.read_only()
    } else {
        spec
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_core::future::BoxFuture;

    use super::*;
    use crate::console::message::{Call, Error, InitResp, Method, Notification, Response};

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

    /// A session taken, which is the whole of what a server owes an `init`.
    fn initialized() -> Response {
        Response::Init(InitResp::default())
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
            Error::UNSUPPORTED_MOUNT,
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

    /// The mounts this end holds are what `init` names, each as one string carrying where
    /// the tree is, where the session sees it, and whether a command may write in it.
    ///
    /// The mount point and the guest path are different here on purpose, to make visible
    /// which of them the session is spelled in: the one this end chose for the session, not
    /// the directory the tree happens to sit in on this host.
    #[tokio::test]
    async fn a_console_names_its_mounts_and_where_it_put_them() {
        let (client, log) = recorder(vec![initialized()]);
        let console = Console::builder()
            .client(client)
            .mount_readonly(PathBuf::from("/mnt/project"), "/work")
            .mount(PathBuf::from("/mnt/collected"), "/work/out")
            .build()
            .await
            .unwrap();

        let Call::Init(init) = log.call(0) else {
            panic!("{:?} is not an init", log.call(0));
        };
        assert_eq!(
            init.mounts
                .iter()
                .map(MountSpec::to_string)
                .collect::<Vec<_>>(),
            [
                "file:///mnt/project:/work:ro",
                "file:///mnt/collected:/work/out"
            ]
        );

        // And what a caller joins a `read` or a `write` onto is the guest path it named, in
        // the order it named them.
        assert_eq!(
            console.mounts().collect::<Vec<_>>(),
            [Path::new("/work"), Path::new("/work/out")]
        );
    }

    /// A tree the protocol could not name is not a session: every path the caller would send
    /// for it afterwards would be a guess, so this end says what it lacked rather than
    /// sending a session it cannot spell.
    ///
    /// Both halves are refused before anything goes out, because both are knowable here —
    /// which is why the recorder below is never asked for an answer.
    #[tokio::test]
    async fn a_mount_this_end_cannot_name_is_not_a_session() {
        let (client, log) = recorder(vec![initialized()]);
        let Err(e) = Console::builder()
            .client(client)
            .mount(PathBuf::from("relative/here"), "/work")
            .build()
            .await
        else {
            panic!("a console over a mount point with no URL should not build");
        };
        assert!(e.to_string().contains("cannot be named"), "{e}");
        assert_eq!(log.methods(), [] as [Method; 0]);

        // And a guest path that is not one the session could join onto is the same kind of
        // nothing — it would be relative to a working directory nobody named.
        let (client, _) = recorder(vec![initialized()]);
        let Err(e) = Console::builder()
            .client(client)
            .mount(PathBuf::from("/mnt/here"), "work")
            .build()
            .await
        else {
            panic!("a console over a relative guest path should not build");
        };
        assert!(e.to_string().contains("guest path is absolute"), "{e}");
    }
}
