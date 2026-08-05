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
//! this end has no pending table, no listener, no thread waiting on something it also
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

use std::process::Command;

use anyhow::Context as _;

use crate::console::base::{Client, Failure};
use crate::console::message::{Error, Exec, ExecResult, Outcome, Progress, Start};
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
    /// [`client`](Self::client) with a [`StdioClient`] over `cmd` is the same thing said
    /// longer, and this is the shape a caller reaches for: the program is the only part
    /// of it a caller decides, since the two descriptors the protocol runs on are the
    /// client's.
    ///
    /// Starting it is [`build`](Self::build)'s, not this method's — nothing here starts
    /// anything, and a program that cannot be started is one of the ways building a
    /// console fails.
    pub fn stdio_client(mut self, cmd: Command) -> Self {
        self.client = Some(Box::new(move || {
            let client = StdioClient::new(cmd).context("starting the console server")?;
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
    pub fn build(self) -> anyhow::Result<Console> {
        let client = self
            .client
            .context("a console needs a client to drive its server")?;

        Ok(Console {
            client: client()?,
            execs: self.execs,
            default_timeout_ms: self.default_timeout_ms,
            started: false,
        })
    }
}

/// A console: something to run commands in, and the executables it may call back into.
///
/// [`start`](Self::start) to boot it, an [`exec`](Self::exec) per command,
/// [`stop`](Self::stop) to release what booting took — and another `start` after that if
/// there is more to do. Dropping it ends the session for good.
///
/// ```no_run
/// use std::process::Command;
///
/// use cortex::console::{Console, Exec};
/// use cortex::executable::ExecutableSet;
///
/// # fn main() -> anyhow::Result<()> {
/// // Whichever console server this is: the client starts it and owns it from here.
/// let mut console = Console::builder()
///     .stdio_client(Command::new("cortex-local-console"))
///     .executables(ExecutableSet::new())
///     .default_timeout_ms(30_000)
///     .build()?;
///
/// console.start()?;
///
/// let result = console.exec(Exec {
///     cmd: vec!["sh".into(), "-c".into(), "echo hi".into()],
///     ..Exec::default()
/// })?;
/// assert_eq!(result.stdout, b"hi\n");
///
/// console.stop()?;
///
/// // And the session ends when the console goes — here, at the end of the scope.
/// # Ok(())
/// # }
/// ```
pub struct Console {
    client: Box<dyn Client>,

    /// What a delegated name announced by [`start`](Self::start) resolves to when one
    /// comes back as a [`Delegated`](Progress::Delegated).
    execs: ExecutableSet,

    default_timeout_ms: Option<u64>,

    /// Whether [`start`](Self::start) has been answered and not yet undone.
    ///
    /// Kept here because nothing else keeps it: what a session allows is not a
    /// transport's business, so the channel does not check it.
    started: bool,
}

impl Console {
    pub fn builder() -> ConsoleBuilder {
        ConsoleBuilder::default()
    }

    /// Boot the server, and announce the names it may call back into.
    ///
    /// Returning is the readiness signal: booting is not free, and this is where it is
    /// paid for rather than inside the first command.
    pub fn start(&mut self) -> Result<(), Failure> {
        if self.started {
            return Err(Failure::broken("this console has already started"));
        }

        self.client.start(Start {
            delegated: self.execs.names().map(str::to_string).collect(),
            default_timeout_ms: self.default_timeout_ms,
        })?;
        self.started = true;
        Ok(())
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
    pub fn exec(&mut self, exec: Exec) -> Result<ExecResult, Failure> {
        if !self.started {
            return Err(Failure::broken("this console has not started"));
        }

        let mut progress = self.client.exec(exec)?;
        loop {
            match progress {
                Progress::Done(result) => return Ok(result),
                // The execution owes an answer it has not been given, so nothing else
                // may be asked until this goes back.
                Progress::Delegated(exec) => {
                    progress = self.client.resume(answer(&self.execs, exec))?;
                }
            }
        }
    }

    /// Release what [`start`](Self::start) booted.
    ///
    /// Not the end of anything: the session stays open and another `start` is allowed,
    /// which is what the protocol's `stop` means. Dropping the console is what ends it.
    pub fn stop(&mut self) -> Result<(), Failure> {
        if !std::mem::take(&mut self.started) {
            return Err(Failure::broken("this console has not started"));
        }
        self.client.stop()
    }
}

impl Drop for Console {
    /// End the session: `stop` if it is started, then `quit`, in that order.
    ///
    /// The order is what the `stop` is for — the server owes us nothing once the session
    /// is over, so anything we want undone has to be undone first.
    ///
    /// And ending is only here, because there is no moment where a caller would want it
    /// earlier and a result for it. What a `quit` reports is how the ending went — over
    /// stdio, the server process's exit status — and by then the channel is shut and the
    /// process collected, so nothing can be done about it either way. What having a
    /// channel took beyond this — a process that must not outlive it — is the client's,
    /// and happens when the client is dropped, immediately after.
    fn drop(&mut self) {
        if std::mem::take(&mut self.started) {
            let _ = self.client.stop();
        }
        let _ = self.client.quit();
    }
}

/// What one delegated call comes to: the `params` of the `resume` that answers it.
fn answer(execs: &ExecutableSet, exec: Exec) -> Outcome {
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
    let Some(result) = execs.invoke(&call) else {
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
    match serde_json::to_value(result) {
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

    use super::*;
    use crate::console::message::{Call, Method, Notification};
    use crate::executable::{ExecResult as ExecOutput, Executable};

    /// A client over canned answers, recording every call it was handed.
    ///
    /// The whole `Call` and not just its method, because the delegated calls a console
    /// resolves are only visible in what its `resume`s carried.
    struct Recorder {
        answers: Vec<Outcome>,
        asked: Arc<Mutex<Vec<Call>>>,
    }

    impl Client for Recorder {
        fn call(&mut self, call: Call) -> Result<Outcome, Failure> {
            self.asked.lock().unwrap().push(call);
            if self.answers.is_empty() {
                // Not a panic: `Drop` stops a console best-effort, and a test that has
                // said all it means to say should not have to answer that too.
                return Err(Failure::broken("nothing left to answer with"));
            }
            Ok(self.answers.remove(0))
        }

        fn notify(&mut self, _: Notification) -> Result<(), Failure> {
            self.asked.lock().unwrap().push(Call::Stop);
            Ok(())
        }
    }

    fn recorder(answers: Vec<Outcome>) -> (Recorder, Arc<Mutex<Vec<Call>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        (
            Recorder {
                answers,
                asked: Arc::clone(&asked),
            },
            asked,
        )
    }

    /// The methods that went out, in order — a notification spelled as `Stop` by
    /// `Recorder`, so `quit` is the last one.
    fn methods(asked: &Arc<Mutex<Vec<Call>>>) -> Vec<Method> {
        asked.lock().unwrap().iter().map(Call::method).collect()
    }

    fn null() -> Outcome {
        Outcome::Result(serde_json::Value::Null)
    }

    fn progress(progress: Progress) -> Outcome {
        Outcome::Result(serde_json::to_value(progress).unwrap())
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
        fn exec(&self, call: &ExecCall) -> ExecOutput {
            ExecOutput::ok(format!("hello {}\n", call.args.join(" ")))
        }
    }

    /// A builder needs exactly one thing, and says which when it does not have it.
    #[test]
    fn a_console_needs_something_to_ask() {
        let Err(failure) = Console::builder().build() else {
            panic!("a console with nothing to ask should not build");
        };
        assert!(failure.to_string().contains("needs a client"), "{failure}");
    }

    /// `stdio_client` takes a program, so building is where one that will not start is
    /// reported — and where one that starts is a console like any other.
    #[test]
    fn a_stdio_console_starts_its_server_when_it_is_built() {
        let Err(e) = Console::builder()
            .stdio_client(Command::new("cortex-no-such-console"))
            .build()
        else {
            panic!("a console over a program that does not exist should not build");
        };
        assert!(e.to_string().contains("starting the console server"), "{e}");

        // A program that starts is all building needs; whether it speaks the protocol is
        // `start`'s to find out, and this one is never asked. Dropping it ends the
        // session, which closes its stdin, which is what lets `cat` be waited for.
        Console::builder()
            .stdio_client(Command::new("cat"))
            .build()
            .unwrap();
    }

    /// `start` announces the registered names, and `exec` before it never reaches the
    /// channel at all.
    #[test]
    fn a_session_starts_before_it_runs_anything() {
        let (client, asked) = recorder(vec![null(), ran(b"hi\n"), null()]);
        let mut console = Console::builder()
            .client(client)
            .executables(ExecutableSet::new().register("foo", Greeter))
            .default_timeout_ms(30_000)
            .build()
            .unwrap();

        // Nothing has started, so nothing is asked.
        assert!(console.exec(Exec::default()).is_err());
        assert!(asked.lock().unwrap().is_empty());

        console.start().unwrap();
        // A second start is refused without asking twice.
        assert!(console.start().is_err());

        assert_eq!(console.exec(Exec::default()).unwrap().stdout, b"hi\n");
        console.stop().unwrap();
        // Stopped is not started, so neither a second stop nor an exec goes out.
        assert!(console.stop().is_err());
        assert!(console.exec(Exec::default()).is_err());

        // Ending is the console going away, and nothing else has to happen for it.
        drop(console);

        // An execution that delegated nothing is one round trip: no `resume`.
        assert_eq!(
            methods(&asked),
            [Method::Start, Method::Exec, Method::Stop, Method::Stop]
        );
    }

    /// A delegated call is resolved from the set and sent back as a `resume`, and the
    /// caller sees one result for the one `exec` it asked for.
    #[test]
    fn a_delegated_call_is_resolved_inside_one_exec() {
        let (client, asked) = recorder(vec![
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

        console.start().unwrap();
        assert_eq!(console.exec(Exec::default()).unwrap().stdout, b"done\n");

        assert_eq!(
            methods(&asked),
            [Method::Start, Method::Exec, Method::Resume, Method::Resume]
        );

        // The registered name ran, and its output is what the first `resume` carried.
        let asked = asked.lock().unwrap();
        let Call::Resume(outcome) = &asked[2] else {
            panic!("{:?} is not a resume", asked[2]);
        };
        let result: ExecResult = outcome.clone().take().unwrap();
        assert_eq!(result.stdout, b"hello world\n");

        // The unregistered one is refused rather than run — a server asking for a name
        // it was never given.
        let Call::Resume(outcome) = &asked[3] else {
            panic!("{:?} is not a resume", asked[3]);
        };
        assert_eq!(outcome.error().map(|e| e.code), Some(Error::NOT_EXECUTABLE));
    }
}
