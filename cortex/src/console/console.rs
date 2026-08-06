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
//! It walks that chain. The server's answer is either the execution's
//! [`ExecResult`] or a delegated call; the second is resolved against the
//! [`ExecutableSet`] and sent back as a `resume`, until an answer is the first.
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
//! [`stop`](Console::stop) is not that. It releases what a [`start`](Console::start)
//! booted and leaves the session open for another one, which is a thing a caller does in
//! the middle rather than at the end — and being in the middle, it is awaited and its
//! answer reported like any other call.

use anyhow::Context as _;
use futures_core::future::BoxFuture;
use tokio::process::Command;

use crate::console::base::{Client, Failure};
use crate::console::message::{
    Call, Error, Exec, ExecResult, Notification, Outcome, Progress, Read, ReadResult, Start, Write,
    WriteResult,
};
use crate::console::stdio::StdioClient;
use crate::executable::{ExecCall, ExecutableSet};

/// Assembles a [`Console`] from the parts it needs.
///
/// A builder rather than arguments because a console is going to acquire more of them —
/// volumes to project, limits, a backend of its own choosing — and each should be
/// something a caller can leave out.
///
/// Nothing here starts anything. The channel is described, not driven, until
/// [`Console::start`].
#[derive(Default)]
pub struct ConsoleBuilder {
    /// Not a client but the making of one, because some clients are a process away:
    /// [`stdio_client`](Self::stdio_client) is handed a program and not a channel, and
    /// starting a program can fail. Deferring that to [`build`](Self::build) keeps every
    /// setter infallible and leaves one place where a console either exists or says what
    /// it lacked.
    client: Option<Box<dyn FnOnce() -> anyhow::Result<Box<dyn Client>>>>,
    execs: ExecutableSet,
    default_timeout_ms: Option<u64>,
}

impl ConsoleBuilder {
    /// Drive the server over `client`.
    ///
    /// Anything that asks will do — a [`StdioClient`](crate::console::stdio::StdioClient)
    /// over a server it started, a virtio port into a guest, both ends in one process for
    /// a test. Whatever it took to have a channel is the client's, including a process if
    /// that is what it runs over, so there is nothing else here about where a server is.
    pub fn client(mut self, client: impl Client + 'static) -> Self {
        self.client = Some(Box::new(move || Ok(Box::new(client))));
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

        self.client = Some(Box::new(move || {
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
    /// These are announced to the server by [`start`](Console::start) and resolved here
    /// when one comes back as a [`Delegated`](Progress::Delegated). Leaving it out means
    /// a client with nothing to delegate, which is still a client.
    pub fn executables(mut self, execs: ExecutableSet) -> Self {
        self.execs = execs;
        self
    }

    /// The fallback for executions that set no timeout of their own.
    ///
    /// `None` is no fallback, and then such an execution runs until it finishes, or
    /// forever.
    pub fn default_timeout_ms(mut self, ms: u64) -> Self {
        self.default_timeout_ms = Some(ms);
        self
    }

    /// Fails for the one part that has no default — something to ask — and for whatever
    /// having a channel took: over stdio, a server process that would not start.
    ///
    /// Not `async`, because nothing here waits for anything. It does still want a runtime
    /// under it: a console over [`stdio_client`](Self::stdio_client) starts a process,
    /// and a process is registered with the runtime that will reap it. Building one from
    /// outside a task or `main` is a panic, not an `Err` — the missing runtime is the
    /// caller's own shape and not something the channel could report.
    pub fn build(self) -> anyhow::Result<Console> {
        let client = self
            .client
            .context("a console needs a client to drive its server")?;

        Ok(Console {
            client: client()?,
            execs: self.execs,
            default_timeout_ms: self.default_timeout_ms,
        })
    }
}

/// A console: something to run commands in, and the executables it may call back into.
///
/// [`start`](Self::start) to boot it, an [`exec`](Self::exec) per command,
/// [`stop`](Self::stop) to release what booting took — and another `start` after that if
/// there is more to do. [`close`](Self::close) ends the session and says how that went;
/// dropping it ends the session too.
///
/// ```no_run
/// use cortex::console::Console;
/// use cortex::executable::ExecutableSet;
///
/// # #[tokio::main]
/// # async fn main() -> anyhow::Result<()> {
/// // Whichever console server this is: the client starts it and owns it from here.
/// let mut console = Console::builder()
///     .stdio_client(&["cortex-local-console"])
///     .executables(ExecutableSet::new())
///     .default_timeout_ms(30_000)
///     .build()?;
///
/// console.start().await?;
///
/// // `None`: this command has no opinion about how long it may take, so the console's
/// // default stands. `Some(ms)` is how one says otherwise.
/// let result = console.exec(["sh", "-c", "echo hi"], None).await?;
/// assert_eq!(result.stdout, b"hi\n");
///
/// console.stop().await?;
///
/// // And the session ends when the console goes — here, at the end of the scope. The
/// // server hears `quit` and exits rather than being killed.
/// # Ok(())
/// # }
/// ```
pub struct Console {
    /// A console has one, always. What ending needs is not for this to become absent but
    /// for it to be *replaced* — see [`Console::drop`](Console#impl-Drop-for-Console).
    client: Box<dyn Client>,

    /// What a delegated name announced by [`start`](Self::start) resolves to when one
    /// comes back as a [`Delegated`](Progress::Delegated).
    execs: ExecutableSet,

    default_timeout_ms: Option<u64>,
}

impl Console {
    pub fn builder() -> ConsoleBuilder {
        ConsoleBuilder::default()
    }

    /// Boot the server, and announce the names it may call back into.
    ///
    /// Returning is the readiness signal: booting is not free, and this is where it is
    /// paid for rather than inside the first command.
    ///
    /// Whether a session may be started twice, or run something before it is started at
    /// all, is the server's answer and not a question asked here. A session lives at the
    /// end that has one, and this end only carries what it is told: a second `start` is
    /// whatever that server does with one.
    pub async fn start(&mut self) -> Result<(), Failure> {
        let start = Start {
            delegated: self.execs.names().map(str::to_string).collect(),
            default_timeout_ms: self.default_timeout_ms,
        };
        self.client.start(start).await
    }

    /// Run one command, and return everything it produced.
    ///
    /// Every delegated call the command makes is resolved here, against the
    /// [`ExecutableSet`] this console was built with, before this returns: the server
    /// answers with a [`Delegated`](Progress::Delegated) instead of a result, the name
    /// runs in *this* process, and what it produced goes back as a `resume`. However
    /// many times that happens is not something a caller sees.
    ///
    /// So a caller waits for one thing and gets one thing, and the delegated calls
    /// underneath it are served in the order the command made them.
    ///
    /// The command is an argv — `["echo", "hi"]` — and nothing here consults a shell, so
    /// a caller that wants shell semantics asks for them outright: `["sh", "-c", ".."]`.
    ///
    /// `timeout_ms` bounds this execution alone. `None` is not "no timeout" but "no
    /// opinion": the server falls back to the console's
    /// [`default_timeout_ms`](ConsoleBuilder::default_timeout_ms), and only a console
    /// that set no default either leaves an execution to run until it finishes, or
    /// forever. Whichever bound applies, it covers the delegated calls the command made
    /// on the way — they are part of the execution and not a pause in it.
    ///
    /// Which is the only way to bound one, and the reason the parameter is here rather
    /// than something a caller arranges outside. The chain is a sequence of round trips on
    /// one channel, and dropping this future between two of them leaves the server holding
    /// an execution nobody will resume — so a caller that wants to stop waiting says so to
    /// the server, which is the end that can also stop the command.
    pub async fn exec(
        &mut self,
        cmd: impl IntoIterator<Item = impl AsRef<str>>,
        timeout_ms: Option<u64>,
    ) -> Result<ExecResult, Failure> {
        let exec = Exec {
            cmd: cmd.into_iter().map(|s| s.as_ref().to_string()).collect(),
            timeout_ms,
        };

        // Split rather than borrowed through `self`, because the chain holds the client
        // across every step and consults the set in between.
        let Console { client, execs, .. } = self;

        let mut progress = client.exec(exec).await?;
        loop {
            match progress {
                Progress::Done(result) => return Ok(result),
                // The execution owes an answer it has not been given, so nothing else
                // may be asked until this goes back.
                Progress::Delegated(exec) => {
                    let outcome = answer(execs, exec).await;
                    progress = client.resume(outcome).await?;
                }
            }
        }
    }

    /// Read part of a file where commands run.
    ///
    /// The path is the one a command would open by the same name, so this is how a
    /// caller sees what an execution wrote to a file rather than to its output.
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
    /// a command will find it.
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

    /// Release what [`start`](Self::start) booted.
    ///
    /// Not the end of anything: the session stays open and another `start` is allowed,
    /// which is what the protocol's `stop` means. [`close`](Self::close) is what ends it.
    ///
    /// Like [`start`](Self::start), what a `stop` on a session that has none comes to is
    /// the server's to say.
    pub async fn stop(&mut self) -> Result<(), Failure> {
        self.client.stop().await
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
            fn call(&mut self, _: Call) -> BoxFuture<'_, Result<Outcome, Failure>> {
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

/// What one delegated call comes to: the `params` of the `resume` that answers it.
async fn answer(execs: &ExecutableSet, exec: Exec) -> Outcome {
    let Some((name, args)) = exec.cmd.split_first() else {
        return refused(Error::INVALID_PARAMS, "an empty command");
    };

    let call = ExecCall {
        name: name.clone(),
        args: args.to_vec(),
    };

    // `None` is the allowlist boundary. Nothing honest reaches it — the only names the
    // server was given are the ones in this set — so this is a server asking for
    // something it was never told about.
    let Some(result) = execs.invoke(&call).await else {
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
    };
    match bson::serialize_to_bson(&result) {
        Ok(value) => Outcome::Result(value),
        Err(e) => refused(Error::INTERNAL_ERROR, format!("encoding a result: {e}")),
    }
}

fn refused(code: i64, message: impl Into<String>) -> Outcome {
    Outcome::Error(Error::new(code, message))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures_core::future::BoxFuture;

    use super::*;
    use crate::console::message::{Call, Method, Notification};
    use crate::executable::{ExecResult as ExecOutput, Executable};

    /// What a [`Recorder`] was handed, readable while it is still lent out.
    ///
    /// Two lists because they answer different questions: `methods` is what went out and
    /// in what order, including the `quit`, which is a notification and so never a `Call`;
    /// `calls` is what those carried, because the delegated calls a console resolves are
    /// only visible in the payload of its `resume`s.
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
        log: Log,
    }

    impl Client for Recorder {
        fn call(&mut self, call: Call) -> BoxFuture<'_, Result<Outcome, Failure>> {
            self.log.methods.lock().unwrap().push(call.method());
            self.log.calls.lock().unwrap().push(call);
            let answer = if self.answers.is_empty() {
                // Not a panic: an ending stops a console best-effort, and a test that has
                // said all it means to say should not have to answer that too.
                Err(Failure::broken("nothing left to answer with"))
            } else {
                Ok(self.answers.remove(0))
            };
            Box::pin(async move { answer })
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
                log: log.clone(),
            },
            log,
        )
    }

    fn null() -> Outcome {
        Outcome::Result(bson::Bson::Null)
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
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            ..Exec::default()
        }))
    }

    struct Greeter;

    impl Executable for Greeter {
        fn exec<'a>(&'a self, call: &'a ExecCall) -> BoxFuture<'a, ExecOutput> {
            Box::pin(async move { ExecOutput::ok(format!("hello {}\n", call.args.join(" "))) })
        }
    }

    /// A builder needs exactly one thing, and says which when it does not have it.
    #[tokio::test]
    async fn a_console_needs_something_to_ask() {
        let Err(failure) = Console::builder().build() else {
            panic!("a console with nothing to ask should not build");
        };
        assert!(failure.to_string().contains("needs a client"), "{failure}");
    }

    /// `stdio_client` takes a program, so building is where one that will not start is
    /// reported — and where one that starts is a console like any other.
    #[tokio::test]
    async fn a_stdio_console_starts_its_server_when_it_is_built() {
        let Err(e) = Console::builder()
            .stdio_client(&["cortex-no-such-console"])
            .build()
        else {
            panic!("a console over a program that does not exist should not build");
        };
        assert!(e.to_string().contains("starting the console server"), "{e}");

        // A command with nothing to run is the same kind of failure and reported the same
        // way: at build, saying what it lacked.
        let Err(e) = Console::builder().stdio_client(&[] as &[&str]).build() else {
            panic!("a console over no program at all should not build");
        };
        assert!(e.to_string().contains("needs a program to run"), "{e}");

        // A program that starts is all building needs; whether it speaks the protocol is
        // `start`'s to find out, and this one never answers. Dropped rather than closed,
        // because `cat` echoes whatever the ending says to it and an ending that reports
        // what it heard would report that — see `close`, which is the ending with a
        // result, and this program has no result to give.
        //
        // Arguments are the caller's too, and go to the program as given.
        drop(
            Console::builder()
                .stdio_client(&["cat", "-u"])
                .build()
                .unwrap(),
        );
    }

    /// `start` announces the registered names, and nothing about the session is kept on
    /// this side: what a caller asks for goes out as asked, in that order, and the answer
    /// is the server's.
    #[tokio::test]
    async fn a_session_is_the_servers_to_keep() {
        let (client, log) = recorder(vec![ran(b"early\n"), null(), ran(b"hi\n"), null()]);
        let mut console = Console::builder()
            .client(client)
            .executables(ExecutableSet::new().register("foo", Greeter))
            .default_timeout_ms(30_000)
            .build()
            .unwrap();

        // An `exec` before any `start` is not refused here. Whether a session has to be
        // started before it runs anything is the server's rule to keep, so this goes out
        // and what comes back is whatever that server says — here, a result.
        assert_eq!(
            console.exec(["echo", "early"], None).await.unwrap().stdout,
            b"early\n"
        );

        console.start().await.unwrap();
        // An execution that delegated nothing is one round trip: no `resume`.
        assert_eq!(
            console.exec(["echo", "hi"], None).await.unwrap().stdout,
            b"hi\n"
        );
        // A `stop` in the middle: what booting took is released and the session stays
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
                Method::Exec,
                Method::Start,
                Method::Exec,
                Method::Stop,
                Method::Quit
            ]
        );

        // What `start` carried: the registered names, and the fallback timeout.
        let Call::Start(start) = log.call(1) else {
            panic!("{:?} is not a start", log.call(1));
        };
        assert_eq!(start.delegated, ["foo"]);
        assert_eq!(start.default_timeout_ms, Some(30_000));
    }

    /// A console that was never started is ended the same way, because what `quit` is for
    /// is the server going away and a server exists whether or not it was ever booted.
    #[tokio::test]
    async fn dropping_a_console_ends_its_session() {
        let (client, log) = recorder(vec![null()]);
        let console = Console::builder().client(client).build().unwrap();

        drop(console);

        // The ending is somebody else's turn to run, so wait for one.
        tokio::task::yield_now().await;
        assert_eq!(log.methods(), [Method::Quit]);
    }

    /// A delegated call is resolved from the set and sent back as a `resume`, and the
    /// caller sees one result for the one `exec` it asked for.
    #[tokio::test]
    async fn a_delegated_call_is_resolved_inside_one_exec() {
        let (client, log) = recorder(vec![
            null(),
            // Two in a row, so the chain is a loop and not a single extra step.
            delegated(&["foo", "world"]),
            delegated(&["nope"]),
            ran(b"done\n"),
        ]);
        let mut console = Console::builder()
            .client(client)
            .executables(ExecutableSet::new().register("foo", Greeter))
            .build()
            .unwrap();

        console.start().await.unwrap();
        assert_eq!(console.exec(["foo"], None).await.unwrap().stdout, b"done\n");

        assert_eq!(
            log.methods(),
            [Method::Start, Method::Exec, Method::Resume, Method::Resume]
        );

        // The registered name ran, and its output is what the first `resume` carried.
        let Call::Resume(outcome) = log.call(2) else {
            panic!("{:?} is not a resume", log.call(2));
        };
        let result: ExecResult = outcome.take().unwrap();
        assert_eq!(result.stdout, b"hello world\n");

        // The unregistered one is refused rather than run — a server asking for a name
        // it was never given.
        let Call::Resume(outcome) = log.call(3) else {
            panic!("{:?} is not a resume", log.call(3));
        };
        assert_eq!(outcome.error().map(|e| e.code), Some(Error::NOT_EXECUTABLE));
    }
}
