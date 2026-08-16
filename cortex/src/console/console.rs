//! The public end: a console server to run commands in, and the one channel it takes.
//!
//! # One channel, one direction of asking
//!
//! A console is one conversation. This end asks and the server answers, and that stays
//! true even for a *delegated* executable — a name whose behaviour lives out here,
//! called by something running inside the server.
//!
//! The server does not ask for one. It *answers* with a
//! [`Delegated`](Progress::Delegated): a complete response to the `exec` this end is
//! already waiting on, meaning the execution is not over and here is what it needs. So
//! this end has no pending table, no listener, no task waiting on something it also
//! has to read, and no channel per delegated call.
//!
//! # What [`exec`](Console::exec) actually does
//!
//! It walks that chain. The server's answer is either the execution's [`ExecResult`] or a
//! delegated call; the second is resolved against the [`ExecutableSet`] and carried back
//! on an `exec` of its own, until an answer is the first.
//!
//! Which is why the loop is here and not in a transport: `Progress` is a
//! [`Client`]'s and resolving a *name* is an [`ExecutableSet`]'s, and this is the
//! one type that holds both.
//!
//! Delegated calls are therefore served in turn, not together — a command that starts
//! several (`foo & bar`, `make -j8`) has them run one after another. See
//! [`Progress`] for why that is latency rather than a deadlock, and what would have to
//! change for it to stop being so.
//!
//! # Waiting is the point, and a console does none of it in a thread
//!
//! Everything here is something to `await`: a command runs for as long as it runs, and a
//! delegated executable is usually a call out to something slower still. A caller with
//! several consoles wants all of them going at once, which is what an async API gives
//! for the price of a task each — where a thread each would have been a thread parked on
//! a pipe.
//!
//! What is *not* concurrent is one console. Its methods take `&mut self`, because the
//! protocol is one outstanding request at a time and a delegated execution owes an answer
//! before anything else may be asked — see [`Client`] for why an exclusive borrow is the
//! honest way to say that.
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
            Call, Error, Exec, ExecCmd, ExecResult, Init, Notification, Outcome, Progress, Read,
            ReadResult, RequestId, WorkFsSource, Write, WriteResult,
        },
        stdio::StdioClient,
    },
    executable::{ExecCall, ExecutableSet, relativize},
    fs::Mount,
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

    execs: ExecutableSet,

    /// `None` is a console with nothing mounted, and stays `None`: a mount is something a
    /// caller *has* or has not, and there is no empty one to substitute — an unmounted tree
    /// has no path for a delegated name to open. What that means for the names is
    /// [`Executable::exec`](crate::executable::Executable::exec)'s to say.
    mount: Option<Box<dyn Mount>>,
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

    /// The names this console offers, and what each one does.
    ///
    /// These are announced to the server as the console is built, and resolved here when
    /// one comes back as a [`Delegated`](Progress::Delegated). Leaving it out means a
    /// client with nothing to delegate, which is still a client.
    pub fn executables(mut self, execs: ExecutableSet) -> Self {
        self.execs = execs;
        self
    }

    /// Where this console's tree is mounted, which is what its delegated executables are
    /// handed — and what the session's workfs is.
    ///
    /// One per console and fixed for the session, because it is what an execution's
    /// delegated calls are resolved *against*: a name that reads a file is asked about the
    /// tree the command that called it can see, and it can see it because it is mounted.
    ///
    /// Mounting is the caller's, not the console's. Which binding puts a tree in front of a
    /// kernel is a build's business, and this is the whole of what the server is then told
    /// about it: [`build`](Self::build) names this mount point as the session's workfs —
    /// `file://` and the path — and the server answers where *it* plugged that in. The two
    /// are usually the same path and nothing requires them to be, which is why the answer
    /// is read rather than assumed; see [`WorkFsMount`](crate::console::WorkFsMount).
    ///
    /// Takes the mount by value, so a console holds it and the mount lives at least as long
    /// as the session does — dropping a mount unmounts it (see [`Mount`]). A caller that
    /// needs it elsewhere as well hands over an `Arc<..>` of it, which is a [`Mount`] too:
    /// how long a mount lives is the lifetime of the value, and whose lifetime that is, is
    /// the caller's to decide rather than something this end arranges behind it.
    ///
    /// Leaving it out is a console with nothing mounted; see the field this fills.
    pub fn mount(mut self, mount: impl Mount + 'static) -> Self {
        self.mount = Some(Box::new(mount));
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
    pub async fn build(self) -> anyhow::Result<Console> {
        Console::new(self).await
    }
}

/// A console: something to run commands in, and the executables it may call back into.
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
/// use cortex::executable::ExecutableSet;
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// // Whichever console server this is: the client starts it and owns it from here.
/// // Building also says what the session is — the delegated names, and the tree it works
/// // in if there is one — so a console that exists is one the server has answered.
/// // Nothing is booted by that; the command below pays for the boot, unless a `start`
/// // gets there first.
/// let mut console = Console::builder()
///     .stdio_client(&["cortex-local-console"])
///     .executables(ExecutableSet::new())
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
pub struct Console {
    /// A console has one, always. What ending needs is not for this to become absent but
    /// for it to be *replaced* — see [`Console::drop`](Console#impl-Drop-for-Console).
    client: Box<dyn Client>,

    /// What a delegated name — announced to the server when this console was built —
    /// resolves to when one comes back as a [`Delegated`](Progress::Delegated).
    execs: ExecutableSet,

    /// The mount those names are resolved against, handed to each one as it runs — `None`
    /// when this console has nothing mounted.
    ///
    /// Held for the session, which is also what keeps the mount up: dropping it unmounts.
    /// Whether this is the last holder is the caller's business — what arrives here is
    /// whatever was passed to [`ConsoleBuilder::mount`], an `Arc<..>` included.
    mount: Option<Box<dyn Mount>>,

    /// Where the server put that same tree, as it answered at `init` — `None` alongside a
    /// `mount` that is `None`, since there was no workfs to put anywhere.
    ///
    /// **The paths this protocol speaks are these.** A [`read`](Self::read) names a file
    /// under this, and a reported [`cwd`](Exec::cwd) is a directory under it. What a
    /// delegated executable is handed instead is the same directory named from the root of
    /// the tree, because that is the form that means the same thing through the mount above
    /// — usually the very same directory, and nothing requires the two ends to have it at
    /// one path.
    ///
    /// Where the session *stands* is not here and is not kept anywhere on this side: the
    /// current directory is the far end's state machine, `init` says where it starts and
    /// [`ExecResult::cwd`] says when an execution moved it, so a copy here would be a
    /// second answer to a question that already has one — wrong from the first `cd`
    /// somebody else's execution ran.
    server_path: Option<PathBuf>,
}

impl Console {
    pub fn builder() -> ConsoleBuilder {
        ConsoleBuilder::default()
    }

    /// Take a channel and announce the session on it.
    ///
    /// `init` is here rather than a method a caller remembers, because a session's shape
    /// is not something a console is ever without: the delegated names and the tree are
    /// what the builder was given, they outlive every execution, and there is no useful
    /// console in between having a channel and having said what is on it. So a `Console`
    /// that exists is one the server has heard from and answered — which is the one thing
    /// about a session a caller can act on before asking for work.
    ///
    /// The answer is not only an acknowledgement: it says where the server put the workfs,
    /// and that path is what every later `read`, `write` and reported `cwd` is spelled in.
    /// A server that takes a workfs and says nothing about where it went has left this end
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
            execs,
            mount,
        } = builder;

        let client_factory =
            client_factory.context("a console needs a client to drive its server")?;
        let mut client = client_factory()?;

        // What the server is told about the tree is where this end has it. A mount point
        // with no URL to it is refused here rather than sent, by the rule the client factory
        // above follows: a console either exists or says what it lacked.
        let workfs = match mount.as_deref() {
            Some(mount) => Some(WorkFsSource::new(mount.url().with_context(|| {
                format!(
                    "a mount point that is not an absolute UTF-8 path cannot be named as a \
                     workfs: {:?}",
                    mount.mountpoint()
                )
            })?)),
            None => None,
        };

        let answered = client
            .init(Init {
                delegated: execs.names().map(str::to_string).collect(),
                workfs,
            })
            .await?;

        let server_path = match (&mount, answered.workfs) {
            (Some(_), Some(at)) => Some(absolute(at.path)?),
            (Some(mount), None) => anyhow::bail!(
                "the console server took the workfs at {} and did not say where it put it",
                mount.mountpoint().display()
            ),
            // Nothing was asked for, so a path that came back anyway is about a session
            // this end did not describe, and there is nothing here it could name.
            (None, _) => None,
        };

        Ok(Console {
            client,
            execs,
            mount,
            server_path,
        })
    }

    /// Where the server put this session's tree, and so what every path this console sends
    /// is relative to — `None` when nothing is mounted.
    ///
    /// A caller builds a [`read`](Self::read) or a [`write`](Self::write) path by joining
    /// onto this. It is the server's answer and not the mount point this end passed in: the
    /// two are usually the same directory, and which one the protocol speaks in is settled
    /// rather than assumed.
    pub fn workfs_path(&self) -> Option<&Path> {
        self.server_path.as_deref()
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
    /// needed one, as [`BOOT_FAILED`](Error::BOOT_FAILED).
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
    /// Every delegated call the command makes is resolved here, against the
    /// [`ExecutableSet`] this console was built with, before this returns: the server
    /// answers with a [`Delegated`](Progress::Delegated) instead of a result, the name
    /// runs in *this* process, and what it produced goes back as the
    /// [`Resume`](ExecCmd::Resume) of another `exec`. However many times that happens is
    /// not something a caller sees.
    ///
    /// So a caller waits for one thing and gets one thing, and the delegated calls
    /// underneath it are served in the order the command made them.
    ///
    /// The command is an argv — `["echo", "hi"]` — and nothing here consults a shell, so
    /// a caller that wants shell semantics asks for them outright: `["sh", "-c", ".."]`.
    ///
    /// `timeout_ms` bounds this execution, and `None` leaves it to run until it finishes
    /// — or forever, if it is the kind of command that does not finish. Whichever it is,
    /// it covers the delegated calls the command made on the way: they are part of the
    /// execution and not a pause in it.
    ///
    /// Which is the only way to bound one, and the reason the parameter is here rather
    /// than something a caller arranges outside. The chain is a sequence of round trips on
    /// one channel, and dropping this future between two of them leaves the server holding
    /// an execution nobody will carry on — so a caller that wants to stop waiting says so
    /// to the server, which is the end that can also stop the command.
    pub async fn exec(
        &mut self,
        cmd: impl IntoIterator<Item = impl AsRef<str>>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecResult, Failure> {
        let exec = Exec {
            // A command a caller asked for, so it carries on from nothing.
            cmd: ExecCmd::New(cmd.into_iter().map(|s| s.as_ref().to_string()).collect()),
            timeout_ms,
            // A caller asking for a command names no directory: where an execution runs is
            // the server's, decided by the namespace it mounted. The field reports where
            // one *did* run, which only the end that ran it can say.
            cwd: None,
        };

        // Split rather than borrowed through `self`, because the chain holds the client
        // across every step and consults the set — and the mount it resolves against —
        // in between.
        let Console {
            client,
            execs,
            mount,
            server_path,
        } = self;

        // The id of the request the *next* answer will be to, which is what carrying on
        // from a delegated call has to quote. Every step of the chain is a request of its
        // own, so it moves along with the chain.
        let (mut id, progress) = client.exec(exec).await;
        let mut progress = progress?;

        loop {
            match progress {
                Progress::Done(result) => return Ok(result),

                // The execution owes an answer it has not been given, so nothing else
                // may be asked until this goes back — as an `exec` of its own, with no
                // command of its own to run.
                Progress::Delegated(mut delegated) => {
                    // The directory the server reported is a path in *its* filesystem, and
                    // what resolves an executable's arguments is that directory named from
                    // the root of the tree — the one form that means the same file through
                    // this end's own mount. A directory outside the tree has no such name,
                    // and `None` is what says so.
                    delegated.cwd = match (server_path.as_deref(), delegated.cwd.as_deref()) {
                        (Some(root), Some(cwd)) => relativize(Path::new(cwd), root),
                        _ => None,
                    };

                    let carry_on = Exec {
                        cmd: ExecCmd::Resume {
                            id,
                            outcome: answer(execs, mount.as_deref(), delegated).await,
                        },
                        // The execution this belongs to is already running under its own,
                        // and this is not a second execution to bound.
                        timeout_ms: None,
                        // Nor a command to be run anywhere: this carries an answer back.
                        cwd: None,
                    };

                    let (next, answered) = client.exec(carry_on).await;
                    (id, progress) = (next, answered?);
                }
            }
        }
    }

    /// Read part of a file where commands run.
    ///
    /// The path is the server's — under [`workfs_path`](Self::workfs_path), which is what a
    /// caller joins onto — so this names the file a command would open by the same name, and
    /// is how a caller sees what an execution wrote to a file rather than to its output.
    ///
    /// `offset` is where to start, `None` being the beginning; `len` is how much to ask
    /// for, `None` being the rest. Neither is a promise about what comes back — a file
    /// too large for one message arrives in pieces, and [`ReadResult::size`] against what
    /// did arrive is the only thing that says there are more.
    pub async fn read(
        &mut self,
        path: impl AsRef<str>,
        offset: Option<u64>,
        len: Option<u64>,
    ) -> Result<ReadResult, Failure> {
        let read = Read {
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
    ) -> Result<WriteResult, Failure> {
        let write = Write {
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
            /// Id zero because nothing was numbered: no request went out for this to be
            /// the id of.
            fn call(&mut self, _: Call) -> BoxFuture<'_, (RequestId, Result<Outcome, Failure>)> {
                Box::pin(async { (0, Err(Failure::broken("the session has ended"))) })
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

/// What one delegated call comes to: the outcome of the [`ExecCmd::Resume`] that carries
/// it back.
async fn answer(execs: &ExecutableSet, mount: Option<&dyn Mount>, exec: Exec) -> Outcome {
    let Some((name, args)) = exec.cmd.split() else {
        return refused(Error::INVALID_PARAMS, "an empty command");
    };

    let call = ExecCall {
        name: name.clone(),
        args: args.to_vec(),
        // Where the command that invoked this stood, as the server reported it —
        // workspace-relative, so it means the same thing against this side's own tree.
        cwd: exec.cwd.clone(),
    };

    // `None` is the allowlist boundary. Nothing honest reaches it — the only names the
    // server was given are the ones in this set — so this is a server asking for
    // something it was never told about.
    let Some(result) = execs.invoke(&call, mount).await else {
        return refused(
            Error::NOT_EXECUTABLE,
            format!("{}: not a delegated executable", call.name),
        );
    };

    if result.timed_out {
        return refused(Error::TIMED_OUT, format!("{}: timed out", call.name));
    }

    let result = ExecResult {
        code: result.exit_code,
        stdout: result.stdout,
        stderr: result.stderr,
        truncated: false,
        // A name that ran out here did not move the session, and where that session stands
        // is not something this end knows in the first place.
        cwd: None,
    };
    match bson::serialize_to_bson(&result) {
        Ok(value) => Outcome::Result(value),
        Err(e) => refused(Error::INTERNAL_ERROR, format!("encoding a result: {e}")),
    }
}

fn refused(code: i64, message: impl Into<String>) -> Outcome {
    Outcome::Error(Error::new(code, message))
}

/// The server's answer, as a path this end can join onto.
///
/// A relative one is refused rather than resolved: it would be relative to a working
/// directory the server never named, and every path built from it would name a file nobody
/// meant.
fn absolute(path: String) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(path);
    anyhow::ensure!(
        path.is_absolute(),
        "the console server put the workfs somewhere that is not an absolute path: {}",
        path.display()
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_core::future::BoxFuture;

    use super::*;
    use crate::{
        console::message::{Call, InitResult, Method, Notification, WorkFsMount},
        executable::{ExecResult as ExecOutput, Executable},
    };

    /// What a [`Recorder`] was handed, readable while it is still lent out.
    ///
    /// Two lists because they answer different questions: `methods` is what went out and
    /// in what order, including the `quit`, which is a notification and so never a `Call`;
    /// `calls` is what those carried, because the delegated calls a console resolves are
    /// only visible in the `cmd` of the `exec`s that carry them back.
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
        answers: Vec<Outcome>,
        /// Numbered as a real client numbers: from zero, by one, over calls alone. What
        /// a console quotes back in a resume comes from here, so it has to be the same
        /// sequence a transport would have produced.
        next_id: RequestId,
        log: Log,
    }

    impl Client for Recorder {
        fn call(&mut self, call: Call) -> BoxFuture<'_, (RequestId, Result<Outcome, Failure>)> {
            let id = self.next_id;
            self.next_id += 1;

            self.log.methods.lock().unwrap().push(call.method());
            self.log.calls.lock().unwrap().push(call);
            let answer = if self.answers.is_empty() {
                // Not a panic: an ending stops a console best-effort, and a test that has
                // said all it means to say should not have to answer that too.
                Err(Failure::broken("nothing left to answer with"))
            } else {
                Ok(self.answers.remove(0))
            };
            Box::pin(async move { (id, answer) })
        }

        fn notify(&mut self, notification: Notification) -> BoxFuture<'_, Result<(), Failure>> {
            self.log.methods.lock().unwrap().push(notification.method());
            Box::pin(async { Ok(()) })
        }
    }

    fn recorder(answers: Vec<Outcome>) -> (Recorder, Log) {
        let log = Log {
            methods: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        (
            Recorder {
                answers,
                next_id: 0,
                log: log.clone(),
            },
            log,
        )
    }

    /// A session taken with nothing mounted, which is what a server answers a workfs-less
    /// `init` with.
    fn initialized() -> Outcome {
        Outcome::Result(bson::serialize_to_bson(&InitResult::default()).unwrap())
    }

    /// A session taken, with the workfs put at `path`.
    fn initialized_at(path: &Path) -> Outcome {
        Outcome::Result(
            bson::serialize_to_bson(&InitResult {
                workfs: Some(WorkFsMount {
                    path: path.to_str().expect("a test path is UTF-8").to_string(),
                }),
                ..InitResult::default()
            })
            .unwrap(),
        )
    }

    fn progress(progress: Progress) -> Outcome {
        Outcome::Result(bson::serialize_to_bson(&progress).unwrap())
    }

    /// An execution that finished without delegating anything.
    fn ran(stdout: &[u8]) -> Outcome {
        progress(Progress::Done(ExecResult {
            code: 0,
            stdout: stdout.to_vec(),
            ..ExecResult::default()
        }))
    }

    /// An execution pausing on a delegated name.
    fn delegated(cmd: &[&str]) -> Outcome {
        progress(Progress::Delegated(Exec {
            cmd: ExecCmd::New(cmd.iter().map(|s| s.to_string()).collect()),
            ..Exec::default()
        }))
    }

    /// An execution pausing on a delegated name, invoked in `cwd`.
    fn delegated_in(cmd: &[&str], cwd: &str) -> Outcome {
        progress(Progress::Delegated(Exec {
            cmd: ExecCmd::New(cmd.iter().map(|s| s.to_string()).collect()),
            cwd: Some(cwd.to_string()),
            ..Exec::default()
        }))
    }

    /// A directory that is already part of a filesystem, standing in for a mounted one.
    ///
    /// Not a mount these tests made: putting one up needs a binding, a libfuse provider and
    /// a kernel, which is what `tests/host_mount.rs` is for. What is under test here is what
    /// a *console* does with a mount — name it to the server, hand it to the names it
    /// delegates — and a plain directory answers a path the same way a mount point does.
    struct Mounted(std::path::PathBuf);

    impl Mount for Mounted {
        fn mountpoint(&self) -> &Path {
            &self.0
        }
    }

    struct Greeter;

    impl Executable for Greeter {
        fn exec<'a>(
            &'a self,
            call: &'a ExecCall,
            _mount: Option<&'a dyn Mount>,
        ) -> BoxFuture<'a, ExecOutput> {
            Box::pin(async move { ExecOutput::ok(format!("hello {}\n", call.args.join(" "))) })
        }
    }

    /// Reports what it was told about where it ran, so a test can see whether anything
    /// arrived — and, since it resolves an argument, whether the directory is usable.
    struct Where;

    impl Executable for Where {
        fn exec<'a>(
            &'a self,
            call: &'a ExecCall,
            _mount: Option<&'a dyn Mount>,
        ) -> BoxFuture<'a, ExecOutput> {
            Box::pin(async move {
                match call.resolve(&call.args[0]) {
                    Ok(path) => ExecOutput::ok(path.to_string_lossy().into_owned()),
                    Err(e) => ExecOutput::failed(1, format!("{e}")),
                }
            })
        }
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

    /// Building announces the registered names, and nothing about the session is kept on
    /// this side afterwards: what a caller asks for goes out as asked, in that order, and
    /// the answer is the server's.
    #[tokio::test]
    async fn a_session_is_the_servers_to_keep() {
        let (client, log) = recorder(vec![initialized(), ran(b"hi\n")]);
        let mut console = Console::builder()
            .client(client)
            .executables(ExecutableSet::new().register("foo", "greet the arguments", Greeter))
            .build()
            .await
            .unwrap();

        // Optional, and unanswered: only the timing of the boot below changes.
        console.start().await.unwrap();
        // An execution that delegated nothing is one round trip.
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

        // What `init` carried: the registered names. It is the first thing on the
        // channel, before anything a caller asked for.
        let Call::Init(init) = log.call(0) else {
            panic!("{:?} is not an init", log.call(0));
        };
        assert_eq!(init.delegated, ["foo"]);
    }

    /// A session the server refuses to have is not a console, so there is nothing for a
    /// caller to hold and nothing to end.
    #[tokio::test]
    async fn a_console_the_server_will_not_have_does_not_exist() {
        let (client, log) = recorder(vec![Outcome::Error(Error::new(
            Error::INVALID_PARAMS,
            "delegated names are not a list",
        ))]);
        let Err(e) = Console::builder().client(client).build().await else {
            panic!("a console whose init was refused should not build");
        };
        assert!(e.to_string().contains("delegated names"), "{e}");

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

    /// A delegated call is resolved from the set and carried back on an `exec` of its
    /// own, and the caller sees one result for the one command it asked for.
    ///
    /// So the chain is `exec`s all the way down, told apart by the shape of the `cmd` they
    /// carry: the caller's is a command, and every one after it is an answer to what the
    /// last response asked for.
    #[tokio::test]
    async fn a_delegated_call_is_resolved_inside_one_exec() {
        let (client, log) = recorder(vec![
            initialized(),
            // Two in a row, so the chain is a loop and not a single extra step.
            delegated(&["foo", "world"]),
            delegated(&["nope"]),
            ran(b"done\n"),
        ]);
        let mut console = Console::builder()
            .client(client)
            .executables(ExecutableSet::new().register("foo", "greet the arguments", Greeter))
            .build()
            .await
            .unwrap();

        assert_eq!(console.exec(["foo"], None).await.unwrap().stdout, b"done\n");

        assert_eq!(
            log.methods(),
            [Method::Init, Method::Exec, Method::Exec, Method::Exec]
        );

        /// The `n`th call, as the resume it must be: which request it carries on from, and
        /// what it has to say about it.
        fn resumed(log: &Log, n: usize) -> (RequestId, Outcome) {
            match log.call(n) {
                Call::Exec(Exec {
                    cmd: ExecCmd::Resume { id, outcome },
                    ..
                }) => (id, outcome),
                other => panic!("{other:?} is not an exec carrying on from anything"),
            }
        }

        // What the caller asked for: a command, carrying on from nothing.
        let Call::Exec(asked) = log.call(1) else {
            panic!("{:?} is not an exec", log.call(1));
        };
        assert_eq!(asked.cmd, ExecCmd::New(vec!["foo".into()]));

        // The registered name ran, and its output is what the next `exec` carried — under
        // the id of the request that asked for it, which is the one above.
        let (id, outcome) = resumed(&log, 2);
        assert_eq!(id, 1);
        let result: ExecResult = outcome.take().unwrap();
        assert_eq!(result.stdout, b"hello world\n");

        // The unregistered one is refused rather than run — a server asking for a name
        // it was never given — and an outcome is what makes saying so possible at all.
        let (id, outcome) = resumed(&log, 3);
        assert_eq!(id, 2);
        assert_eq!(outcome.error().map(|e| e.code), Some(Error::NOT_EXECUTABLE));
    }

    /// A delegated name is handed the console's own mount, so the file it opens is the one
    /// the session's tree has under that name rather than whatever this process's own
    /// directory holds.
    ///
    /// The whole chain is here, because it is the chain that makes the two ends agree: the
    /// directory the server reported — a path in *its* filesystem — stripped back to a name
    /// in the tree, the argument resolved against that, then joined onto the mount point,
    /// and a real file at the end of it.
    ///
    /// The server puts the workfs somewhere of its own here, which is why the strip is
    /// visible: `/srv/served` and the mount point are the same tree, and only the second is
    /// a directory this end can open.
    #[tokio::test]
    async fn a_delegated_call_reads_the_consoles_mount() {
        /// Answers with the contents of the file it was asked for, opened where the mount
        /// says it is.
        struct Cat;

        impl Executable for Cat {
            fn exec<'a>(
                &'a self,
                call: &'a ExecCall,
                mount: Option<&'a dyn Mount>,
            ) -> BoxFuture<'a, ExecOutput> {
                Box::pin(async move {
                    let Some(mount) = mount else {
                        return ExecOutput::failed(1, "nothing is mounted");
                    };
                    let path = match call.resolve(&call.args[0]) {
                        Ok(path) => mount.host_path(&path),
                        Err(e) => return ExecOutput::failed(1, e.to_string()),
                    };
                    match std::fs::read(&path) {
                        Ok(bytes) => ExecOutput::ok(bytes),
                        Err(e) => ExecOutput::failed(1, e.to_string()),
                    }
                })
            }
        }

        let mnt = std::env::temp_dir().join(format!("cortex-console-{}", std::process::id()));
        std::fs::create_dir_all(&mnt).expect("temp dir is writable");
        std::fs::write(mnt.join("note.txt"), b"from the mounted tree\n").unwrap();

        let (client, log) = recorder(vec![
            initialized_at(Path::new("/srv/served")),
            // The root of the tree, as the server names it — which is what lets a relative
            // argument resolve at all, once it has been stripped back to a name in the tree.
            delegated_in(&["cat", "note.txt"], "/srv/served"),
            ran(b"done\n"),
        ]);
        let mut console = Console::builder()
            .client(client)
            .executables(ExecutableSet::new().register(
                "cat",
                "read a file out of the tree it was handed",
                Cat,
            ))
            .mount(Mounted(mnt.clone()))
            .build()
            .await
            .unwrap();

        // The workfs is this end's mount point, and where it ended up is the server's answer.
        let Call::Init(init) = log.call(0) else {
            panic!("{:?} is not an init", log.call(0));
        };
        assert_eq!(
            init.workfs,
            Some(WorkFsSource::new(format!("file://{}", mnt.display())))
        );
        assert_eq!(console.workfs_path(), Some(Path::new("/srv/served")));

        console
            .exec(["sh", "-c", "cat note.txt"], None)
            .await
            .unwrap();
        std::fs::remove_dir_all(&mnt).ok();

        let Call::Exec(Exec {
            cmd: ExecCmd::Resume { outcome, .. },
            ..
        }) = log.call(2)
        else {
            panic!("{:?} is not an exec carrying on from anything", log.call(2));
        };
        let result: ExecResult = outcome.take().unwrap();
        assert_eq!(result.stdout, b"from the mounted tree\n");
    }

    /// What an executable resolves against is the reported directory named from the root of
    /// the tree, which is the form that means the same file on both sides of the channel.
    ///
    /// The server's own path is what arrives, and stripping it is this end's — so a
    /// `/srv/served/docs/sub` on the wire is a `docs/sub` on the call.
    #[tokio::test]
    async fn a_delegated_call_receives_the_directory_it_ran_in() {
        let (client, log) = recorder(vec![
            initialized_at(Path::new("/srv/served")),
            delegated_in(&["where", "report.md"], "/srv/served/docs/sub"),
            ran(b"done\n"),
        ]);
        let mut console = Console::builder()
            .client(client)
            .executables(ExecutableSet::new().register(
                "where",
                "resolve an argument where the command ran",
                Where,
            ))
            .mount(Mounted("/mnt/here".into()))
            .build()
            .await
            .unwrap();

        console.exec(["sh"], None).await.unwrap();

        // The executable resolved its argument against the directory the server reported,
        // which is only possible if the field crossed onto the call — as a name in the tree
        // and not as the server's own path.
        let Call::Exec(Exec {
            cmd: ExecCmd::Resume { outcome, .. },
            ..
        }) = log.call(2)
        else {
            panic!("{:?} is not an exec carrying on from anything", log.call(2));
        };
        let result: ExecResult = outcome.take().unwrap();
        assert_eq!(result.stdout, b"docs/sub/report.md");
    }

    /// And when nothing said where it ran, the executable is told so rather than handed a
    /// root that would name a different file.
    ///
    /// Three ways to arrive at that same `None`, because all three mean the one thing an
    /// executable can act on — there is no directory in this tree that the command stood in:
    /// nothing reported, no tree to name one in, and a directory the tree does not contain.
    #[tokio::test]
    async fn a_delegated_call_with_no_directory_says_so() {
        for (mounted, answer, delegated, why) in [
            (
                true,
                initialized_at(Path::new("/srv/served")),
                delegated(&["where", "report.md"]),
                "nothing was reported",
            ),
            (
                false,
                initialized(),
                delegated_in(&["where", "report.md"], "/srv/served"),
                "there is no tree to name it in",
            ),
            (
                true,
                initialized_at(Path::new("/srv/served")),
                delegated_in(&["where", "report.md"], "/etc"),
                "the command stood outside the tree",
            ),
        ] {
            let (client, log) = recorder(vec![answer, delegated, ran(b"done\n")]);
            let mut builder =
                Console::builder()
                    .client(client)
                    .executables(ExecutableSet::new().register(
                        "where",
                        "resolve an argument where the command ran",
                        Where,
                    ));
            if mounted {
                builder = builder.mount(Mounted("/mnt/here".into()));
            }
            let mut console = builder.build().await.unwrap();

            console.exec(["sh"], None).await.unwrap();

            let Call::Exec(Exec {
                cmd: ExecCmd::Resume { outcome, .. },
                ..
            }) = log.call(2)
            else {
                panic!("{:?} is not an exec carrying on from anything", log.call(2));
            };
            let result: ExecResult = outcome.take().unwrap();
            assert_eq!(result.code, 1, "a relative path is refused when {why}");
            assert!(result.stdout.is_empty(), "{why}");
        }
    }

    /// A session with a workfs needs somewhere to have put it: a path is what every later
    /// call is spelled in, so a server that answers without one has left this end with
    /// nothing it could name, and that is not a console.
    #[tokio::test]
    async fn a_workfs_the_server_did_not_place_is_not_a_session() {
        let (client, _) = recorder(vec![initialized()]);
        let Err(e) = Console::builder()
            .client(client)
            .mount(Mounted("/mnt/here".into()))
            .build()
            .await
        else {
            panic!("a console whose workfs went nowhere should not build");
        };
        assert!(e.to_string().contains("did not say where"), "{e}");

        // And a path that is not one this end could join onto is the same kind of nothing.
        let (client, _) = recorder(vec![initialized_at(Path::new("relative/served"))]);
        let Err(e) = Console::builder()
            .client(client)
            .mount(Mounted("/mnt/here".into()))
            .build()
            .await
        else {
            panic!("a console whose workfs went to a relative path should not build");
        };
        assert!(e.to_string().contains("not an absolute path"), "{e}");
    }
}
