//! The public end: a console server to run commands in, and the two channels it takes.
//!
//! # Why there are two channels
//!
//! A console is not one conversation. Driving the server is one — this end asks, the
//! server answers — and a *delegated* executable is the other, running the opposite
//! way: something inside the server calls a name whose behaviour lives out here, so
//! this end has to answer.
//!
//! They are separate channels rather than one bidirectional channel, which is what
//! keeps each end of each channel doing one job: a [`Requestable`] only asks and a
//! [`Responsable`] only answers, and neither has a pending table or a thread waiting on
//! something it also has to read.
//!
//! # Why delegated calls need threads
//!
//! A delegated call arrives *while* an [`exec`](Console::exec) is outstanding — that is
//! the whole point: the command that triggered it is still running, waiting for the
//! name to produce something. So it cannot be serviced by whoever is blocked on the
//! answer to that `exec`.
//!
//! And there is not one of them but a stream: one shell command can start several
//! delegated executables together (`foo | bar`, `make -j8`), each on a channel of its
//! own and each blocked until it is answered. So a console takes something that *yields*
//! channels — see [`ConsoleBuilder::delegates`] — and gives each one a thread.

use std::io;
use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::Context as _;

use crate::console::base::{Failure, Requestable, Responsable};
use crate::console::message::{Call, Error, Exec, ExecResult, Message, Outcome, Start};
use crate::executable::{ExecCall, ExecutableSet};

/// Assembles a [`Console`] from the parts it needs.
///
/// A builder rather than arguments because a console is going to acquire more of them —
/// volumes to project, limits, a backend of its own choosing — and each should be
/// something a caller can leave out.
///
/// Nothing here starts anything. The two channels are described, not opened, until
/// [`Console::start`].
#[derive(Default)]
pub struct ConsoleBuilder {
    client: Option<Box<dyn Requestable>>,
    server: Option<Child>,
    #[allow(clippy::type_complexity)]
    delegates: Option<Box<dyn FnMut() -> io::Result<Option<Box<dyn Responsable + Send>>> + Send>>,
    execs: ExecutableSet,
    default_timeout_ms: Option<u64>,
}

impl ConsoleBuilder {
    /// Drive the server over `client`.
    ///
    /// Anything that asks will do — a [`StdioRequester`](crate::console::stdio::StdioRequester)
    /// over a child's pipes, a virtio port into a guest, both ends in one process for a
    /// test.
    pub fn client(mut self, client: impl Requestable + 'static) -> Self {
        self.client = Some(Box::new(client));
        self
    }

    /// The process the client talks to, if there is one, so that [`Console::stop`] can
    /// collect it and say how it went.
    ///
    /// Separate from [`client`](Self::client) because they are separate facts: a client
    /// is a channel and this is a process, and a channel does not have to have one.
    pub fn server(mut self, server: Child) -> Self {
        self.server = Some(server);
        self
    }

    /// Answer delegated calls on the channels `accept` yields.
    ///
    /// One call, one channel: a shim reaching this end has exactly one thing to ask, so
    /// `accept` is the thing that waits for the next one — a `UnixListener::accept`, a
    /// virtio port opening, a queue of canned channels in a test. It blocks, is called
    /// again as soon as it returns, and each channel it yields gets a thread.
    ///
    /// `Ok(None)` means no more are coming and this end is done accepting. An `Err` is
    /// reported and stops accepting too: whatever is wrong with a listener is not
    /// something calling it again would fix.
    ///
    /// Leaving it out is allowed and means delegated names are announced but nothing
    /// answers them. Only useful when [`executables`](Self::executables) is empty too.
    pub fn delegates(
        mut self,
        accept: impl FnMut() -> io::Result<Option<Box<dyn Responsable + Send>>> + Send + 'static,
    ) -> Self {
        self.delegates = Some(Box::new(accept));
        self
    }

    /// The names this console offers, and what each one does.
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

    /// Fails only for the one part that has no default: something to ask.
    pub fn build(self) -> anyhow::Result<Console> {
        let client = self
            .client
            .context("a console needs a client to drive its server")?;

        Ok(Console {
            client,
            server: self.server,
            delegates: self.delegates,
            execs: Arc::new(self.execs),
            default_timeout_ms: self.default_timeout_ms,
            serving: None,
            started: false,
        })
    }
}

/// A console: something to run commands in, and the executables it may call back into.
///
/// [`start`](Self::start) to boot it, an [`exec`](Self::exec) per command,
/// [`stop`](Self::stop) to release what booting took — and another `start` after that if
/// there is more to do. [`shutdown`](Self::shutdown) ends the session for good.
///
/// ```no_run
/// use cortex::console::stdio::{StdioRequester, StdioResponder};
/// use cortex::console::{Console, Exec};
/// use cortex::executable::ExecutableSet;
///
/// # fn main() -> anyhow::Result<()> {
/// // Whatever the client channel actually runs over — a child's pipes, a virtio port.
/// # let (to_server, from_server) = (std::io::sink(), std::io::empty());
/// let listener = std::os::unix::net::UnixListener::bind("/tmp/console.sock")?;
///
/// let mut console = Console::builder()
///     .client(StdioRequester::new(from_server, to_server))
///     // One shim connection is one delegated call, so accepting is what waits.
///     .delegates(move || {
///         let (stream, _) = listener.accept()?;
///         let server = StdioResponder::new(stream.try_clone()?, stream);
///         Ok(Some(Box::new(server) as Box<dyn cortex::console::Responsable + Send>))
///     })
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
/// console.shutdown()?;
/// # Ok(())
/// # }
/// ```
pub struct Console {
    client: Box<dyn Requestable>,

    /// The process the client is talking to, if the caller had one to hand over.
    server: Option<Child>,

    /// Taken by [`start`](Self::start) and handed to the thread that accepts on it, which
    /// is why it is an `Option` and not simply a field.
    #[allow(clippy::type_complexity)]
    delegates: Option<Box<dyn FnMut() -> io::Result<Option<Box<dyn Responsable + Send>>> + Send>>,

    /// Shared with that thread, which is the only other place it is read.
    execs: Arc<ExecutableSet>,

    default_timeout_ms: Option<u64>,

    /// The delegate thread, once there is one.
    serving: Option<JoinHandle<()>>,

    /// Whether [`start`](Self::start) has been answered and not yet undone.
    ///
    /// Kept here because nothing else keeps it: what a session allows is not a
    /// transport's business, so neither channel checks it.
    started: bool,
}

impl Console {
    pub fn builder() -> ConsoleBuilder {
        ConsoleBuilder::default()
    }

    /// Boot the server, and begin answering delegated calls.
    ///
    /// Returning is the readiness signal: booting is not free, and this is where it is
    /// paid for rather than inside the first command.
    ///
    /// The delegate thread starts *after* the server has booted, because until then
    /// there is nothing that could call a delegated name.
    pub fn start(&mut self) -> Result<(), Failure> {
        if self.started {
            return Err(Failure::broken("this console has already started"));
        }

        self.client.start(Start {
            delegated: self.execs.names().map(str::to_string).collect(),
            default_timeout_ms: self.default_timeout_ms,
        })?;
        self.started = true;

        // Only the first `start` opens it, because taking it is what leaves nothing to
        // take. A console stopped and started again keeps the thread it already has:
        // whatever accepts did not go anywhere.
        if let Some(accept) = self.delegates.take() {
            let execs = Arc::clone(&self.execs);
            self.serving = Some(std::thread::spawn(move || accepting(accept, execs)));
        }

        Ok(())
    }

    /// Run one command, and return everything it produced.
    pub fn exec(&mut self, exec: Exec) -> Result<ExecResult, Failure> {
        if !self.started {
            return Err(Failure::broken("this console has not started"));
        }
        self.client.exec(exec)
    }

    /// Release what [`start`](Self::start) booted.
    ///
    /// Not the end of anything: the session stays open and another `start` is allowed,
    /// which is what the protocol's `stop` means. [`shutdown`](Self::shutdown) is the one
    /// that ends it.
    pub fn stop(&mut self) -> Result<(), Failure> {
        if !std::mem::take(&mut self.started) {
            return Err(Failure::broken("this console has not started"));
        }
        self.client.stop()
    }

    /// End the session and collect the process if there is one.
    ///
    /// `stop` if it is started, then `quit`, in that order: the server owes us nothing
    /// once the session is over, so anything we want undone has to be undone first. Both
    /// are best-effort — the session is ending either way, and the exit status is the
    /// answer worth having.
    ///
    /// `Ok(None)` when no process was handed over, which is not a failure: a caller that
    /// gave us only a channel gets only the channel's ending.
    ///
    /// Calling this twice is not an error; the second time there is nothing left to do.
    pub fn shutdown(&mut self) -> Result<Option<ExitStatus>, Failure> {
        if std::mem::take(&mut self.started) {
            let _ = self.client.stop();
        }
        let _ = self.client.quit();

        // The delegate thread is not joined here. It ends when whatever it accepts on
        // goes away, and that is not something this function can bring about — a
        // listener with nobody dialling it blocks forever. Joining would hang.
        match self.server.as_mut() {
            Some(server) => server
                .wait()
                .map(Some)
                .context("waiting for the console server")
                .map_err(Failure::Broken),
            None => Ok(None),
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        // A console already shut down by hand takes this as a no-op: `quit` on a closed
        // channel fails, and there is no process left to wait for.
        let _ = self.shutdown();
        // Whatever the session did, the process does not outlive the handle to it.
        if let Some(server) = self.server.as_mut() {
            let _ = server.kill();
            let _ = server.wait();
        }
    }
}

/// Take delegate channels as they arrive and give each one a thread.
///
/// A thread per channel rather than one loop over all of them, because each blocks
/// until its call is answered: several delegated executables can be in flight at once,
/// and serving them in turn would make the first one's runtime the second one's wait.
#[allow(clippy::type_complexity)]
fn accepting(
    mut accept: Box<dyn FnMut() -> io::Result<Option<Box<dyn Responsable + Send>>> + Send>,
    execs: Arc<ExecutableSet>,
) {
    loop {
        match accept() {
            Ok(Some(channel)) => {
                let execs = Arc::clone(&execs);
                std::thread::spawn(move || serve(channel, &execs));
            }
            // Nothing more is coming.
            Ok(None) => return,
            Err(e) => {
                eprintln!("console: no more delegate channels: {e}");
                return;
            }
        }
    }
}

/// Answer calls on one delegate channel until it closes.
///
/// Which is normally exactly one call — a shim asks its single question and goes — but
/// nothing here assumes that.
fn serve(mut delegates: Box<dyn Responsable + Send>, execs: &ExecutableSet) {
    loop {
        let message = match delegates.recv() {
            Ok(Some(message)) => message,
            // The channel ended, cleanly or not. Either way there is nothing to answer
            // and no way to say so.
            Ok(None) => return,
            Err(e) => {
                eprintln!("console: the delegate channel broke: {e}");
                return;
            }
        };

        let Message::Request { id, call } = message else {
            // A response answers a request and this end makes none; a notification on
            // this channel means nothing yet.
            continue;
        };

        if delegates.respond(id, answer(execs, call)).is_err() {
            return;
        }
    }
}

/// What one delegated call comes to.
fn answer(execs: &ExecutableSet, call: Call) -> Outcome {
    let Call::Exec(exec) = call else {
        return refused(
            Error::INVALID_REQUEST,
            "a delegate channel carries executions and nothing else",
        );
    };

    let Some((name, args)) = exec.cmd.split_first() else {
        return refused(Error::INVALID_PARAMS, "an empty command");
    };

    let call = ExecCall {
        name: name.clone(),
        args: args.to_vec(),
    };

    // `None` is the allowlist boundary. Nothing honest reaches it — the only names the
    // server was given are the ones in this set — so this is something that found the
    // channel another way.
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
    use crate::console::message::{Method, Notification, RequestId};
    use crate::console::stdio::{StdioResponder, read, write};
    use crate::executable::{ExecResult as ExecOutput, Executable};

    /// A client over canned answers, recording what it was asked.
    struct Recorder {
        answers: Vec<Outcome>,
        asked: Arc<Mutex<Vec<Method>>>,
    }

    impl Requestable for Recorder {
        fn call(&mut self, call: Call) -> Result<Outcome, Failure> {
            self.asked.lock().unwrap().push(match &call {
                Call::Start(_) => Method::Start,
                Call::Exec(_) => Method::Exec,
                Call::Stop => Method::Stop,
            });
            if self.answers.is_empty() {
                // Not a panic: `Drop` stops a console best-effort, and a test that has
                // said all it means to say should not have to answer that too.
                return Err(Failure::broken("nothing left to answer with"));
            }
            Ok(self.answers.remove(0))
        }

        fn notify(&mut self, _: Notification) -> Result<(), Failure> {
            self.asked.lock().unwrap().push(Method::Quit);
            Ok(())
        }
    }

    fn recorder(answers: Vec<Outcome>) -> (Recorder, Arc<Mutex<Vec<Method>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        (
            Recorder {
                answers,
                asked: Arc::clone(&asked),
            },
            asked,
        )
    }

    fn null() -> Outcome {
        Outcome::Result(serde_json::Value::Null)
    }

    /// What `exec` answers with, which is not null — it has a result shape.
    fn ran(stdout: &[u8]) -> Outcome {
        Outcome::Result(
            serde_json::to_value(ExecResult {
                code: 0,
                stdout: stdout.to_vec(),
                ..ExecResult::default()
            })
            .unwrap(),
        )
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

        // No process was handed over, so there is no status to report.
        assert!(console.shutdown().unwrap().is_none());

        assert_eq!(
            *asked.lock().unwrap(),
            [Method::Start, Method::Exec, Method::Stop, Method::Quit]
        );
    }

    /// A delegated call is answered from the set, on its own channel, while the client
    /// channel is untouched.
    #[test]
    fn a_delegated_call_is_answered_from_the_set() {
        /// Both directions of the delegate channel, as one process's two buffers.
        #[derive(Clone, Default)]
        struct Wrote(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Wrote {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl Wrote {
            /// Wait until `want` responses have been framed here, and return them.
            ///
            /// Polled rather than joined because the threads that write these are one
            /// per channel and nothing hands back a handle to them. Panics rather than
            /// hanging if they never arrive.
            fn responses(&self, want: usize) -> Vec<(RequestId, Outcome)> {
                for _ in 0..1_000 {
                    let bytes = self.0.lock().unwrap().clone();
                    let mut reader = bytes.as_slice();
                    let mut seen = Vec::new();
                    // A partial frame reads as an error, which here means "not yet".
                    while let Ok(Some(message)) = read(&mut reader) {
                        match message {
                            Message::Response { id, outcome } => seen.push((id, outcome)),
                            other => panic!("a delegate end sent {other:?}"),
                        }
                    }
                    if seen.len() >= want {
                        return seen;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                panic!("only ever saw fewer than {want} responses");
            }
        }

        fn asking(id: RequestId, cmd: &[&str]) -> Vec<u8> {
            let mut bytes = Vec::new();
            write(
                &mut bytes,
                &Message::Request {
                    id,
                    call: Call::Exec(Exec {
                        cmd: cmd.iter().map(|s| s.to_string()).collect(),
                        ..Exec::default()
                    }),
                },
            )
            .unwrap();
            bytes
        }

        let mut incoming = asking(0, &["foo", "world"]);
        incoming.extend(asking(1, &["nope"]));

        let answered = Wrote::default();
        let (client, asked) = recorder(vec![null()]);

        // One channel, then nothing more — which is what ends the accept loop.
        let mut once = Some(StdioResponder::new(
            std::io::Cursor::new(incoming),
            answered.clone(),
        ));

        let mut console = Console::builder()
            .client(client)
            .delegates(move || {
                Ok(once
                    .take()
                    .map(|server| Box::new(server) as Box<dyn Responsable + Send>))
            })
            .executables(ExecutableSet::new().register("foo", Greeter))
            .build()
            .unwrap();

        console.start().unwrap();

        // Joining `serving` would only prove the accept loop ended — each channel is
        // served on a thread of its own, so wait for the answers themselves.
        let outcomes = answered.responses(2);
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");

        // The registered name ran, and its output came back as a result.
        assert_eq!(outcomes[0].0, 0);
        let result: ExecResult = outcomes[0].1.clone().take().unwrap();
        assert_eq!(result.stdout, b"hello world\n");

        // The unregistered one is refused, not run.
        assert_eq!(outcomes[1].0, 1);
        assert_eq!(
            outcomes[1].1.error().map(|e| e.code),
            Some(Error::NOT_EXECUTABLE)
        );

        // And none of it went near the client channel.
        assert_eq!(*asked.lock().unwrap(), [Method::Start]);
    }
}
