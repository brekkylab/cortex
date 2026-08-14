//! End-to-end over the real binary: a [`Console`] spawns it, it runs a command on the
//! host, and the delegated executables that command invokes come back here to be
//! answered.
//!
//! Which is the only way to test the interesting part. A delegated executable works only
//! if `execvp` finds a symlink, re-enters this binary as a shim, the shim dials the socket
//! the *server* bound, the server passes the call up the console channel as a `Delegated`,
//! this process answers it with an `exec` whose `cmd` carries the result, and the bytes
//! come back out of the shim's own stdout — four processes and one channel. Nothing
//! smaller than the whole thing exercises it.

use std::path::PathBuf;
use std::process::Stdio;

use cortex::BoxFuture;
use cortex::console::stdio::StdioClient;
use cortex::console::{Console, ExecResult};
use cortex::executable::{ExecCall, ExecResult as ExecOutput, Executable, ExecutableSet};
use cortex::fs::WorkFs;
use tokio::process::Command;

/// An executable with a canned answer — enough to tell a round trip from a coincidence.
struct Fixed {
    stdout: &'static [u8],
    stderr: &'static [u8],
    code: i32,
}

impl Executable for Fixed {
    fn exec<'a>(
        &'a self,
        _call: &'a ExecCall,
        _workspace: &'a WorkFs,
    ) -> BoxFuture<'a, ExecOutput> {
        Box::pin(async move {
            ExecOutput {
                stdout: self.stdout.to_vec(),
                stderr: self.stderr.to_vec(),
                exit_code: self.code,
                timed_out: false,
            }
        })
    }
}

/// Reports everything it was told, to prove the whole call survives the trip.
struct Report;

impl Executable for Report {
    fn exec<'a>(&'a self, call: &'a ExecCall, _workspace: &'a WorkFs) -> BoxFuture<'a, ExecOutput> {
        Box::pin(async move { ExecOutput::ok(format!("{}|{}\n", call.name, call.args.join(","))) })
    }
}

/// How long [`Slow`] takes. Long enough to tell waiting apart from not waiting, short
/// enough that a test suite does not notice.
const SLEEP: std::time::Duration = std::time::Duration::from_millis(50);

/// A delegated name that does what a delegated name is for: waits on something that is
/// not us, and yields while it does rather than holding the thread it was called on.
struct Slow;

impl Executable for Slow {
    fn exec<'a>(
        &'a self,
        _call: &'a ExecCall,
        _workspace: &'a WorkFs,
    ) -> BoxFuture<'a, ExecOutput> {
        Box::pin(async move {
            tokio::time::sleep(SLEEP).await;
            ExecOutput::ok("slept\n")
        })
    }
}

/// A console over the real binary.
///
/// Nothing about the socket the shims dial appears here: the server binds it, names it in
/// the environment of everything it spawns, and cleans it up. This end only asks.
struct Fixture {
    console: Console,
}

impl Fixture {
    async fn new() -> Fixture {
        let execs = ExecutableSet::new()
            .register(
                "foo",
                "answer `bar`",
                Fixed {
                    stdout: b"bar\n",
                    stderr: b"",
                    code: 0,
                },
            )
            .register(
                "boom",
                "run and fail with 3",
                Fixed {
                    stdout: b"",
                    stderr: b"boom failed\n",
                    code: 3,
                },
            )
            .register(
                "rawbytes",
                "answer bytes that are not text",
                Fixed {
                    // Not utf-8, and contains a NUL: nothing may touch it.
                    stdout: &[0xff, 0xfe, 0x00, b'\n'],
                    stderr: b"",
                    code: 0,
                },
            )
            .register("report", "report the call it was made with", Report)
            .register("slow", "wait, then answer", Slow);

        // A `Command` and not `stdio_client`'s argv, because this test wants the server's
        // stderr on ours to read when something fails. Only that is ours to place — the
        // client sets the two descriptors the protocol runs on, and starts the process it
        // owns from here on.
        let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-local-console"));
        server.stderr(Stdio::inherit());
        let client = StdioClient::new(server).expect("starting the console server");

        // Building announces the session, so a fixture that exists is one the server has
        // answered — and the delegated names below are already linked by the time any
        // command runs.
        let console = Console::builder()
            .client(client)
            .executables(execs)
            .build()
            .await
            .expect("building the console");

        Fixture { console }
    }

    /// Run one command and return what it produced.
    ///
    /// No `start` and no `stop` around it, because neither is owed: the command is what
    /// boots the session, and dropping the fixture is what releases it.
    async fn output(&mut self, script: &str) -> ExecResult {
        self.console
            .exec(["sh", "-c", script], None)
            .await
            .expect("running the command")
    }
}

async fn output(script: &str) -> ExecResult {
    Fixture::new().await.output(script).await
}

/// The baseline: nothing delegated is involved, and a real command still runs.
#[tokio::test]
async fn an_ordinary_command_runs_on_the_host() {
    let out = output("echo hello world").await;
    assert_eq!(out.stdout, b"hello world\n");
    assert_eq!(out.code, 0);
}

/// The command's own code comes back as the result's code, not as anything's failure.
#[tokio::test]
async fn a_commands_exit_code_is_a_result_and_not_an_error() {
    let out = output("exit 7").await;
    assert_eq!(out.code, 7);
}

/// The whole point: `foo` is not a file anyone built, and running it produces what this
/// process said it produces.
#[tokio::test]
async fn a_delegated_name_is_answered_by_this_process() {
    let out = output("foo").await;
    assert_eq!(out.stdout, b"bar\n");
    assert_eq!(out.code, 0);
}

/// A delegated executable is a program as far as the shell is concerned, so it composes
/// like one.
#[tokio::test]
async fn a_delegated_name_pipes_like_a_program() {
    let out = output("foo | tr a-z A-Z").await;
    assert_eq!(out.stdout, b"BAR\n");
}

/// Its stderr and its code arrive too, and on the right streams.
#[tokio::test]
async fn a_delegated_name_reports_stderr_and_its_code() {
    let out = output("boom; echo code=$?").await;
    assert_eq!(out.stdout, b"code=3\n");
    assert_eq!(out.stderr, b"boom failed\n");
}

/// Nothing on the way through is text: the bytes come back exactly as they were written,
/// through JSON, a socket, a pipe and a shell.
#[tokio::test]
async fn a_delegated_names_output_is_bytes() {
    let out = output("rawbytes").await;
    assert_eq!(out.stdout, [0xff, 0xfe, 0x00, b'\n']);
}

/// The name it was called by and its arguments survive the trip.
#[tokio::test]
async fn the_call_arrives_as_it_was_made() {
    let out = output("report one two").await;
    assert_eq!(out.stdout, b"report|one,two\n");
}

/// Several started together and served one at a time: each shim waits its turn in the
/// socket's backlog, and all three get their answer.
///
/// Which is the whole of what serialised delegation gives up — the three run in sequence
/// rather than at once — and the whole of what it does not: a delegated call carries no
/// input and so waits on nothing but being served.
#[tokio::test]
async fn delegated_names_started_together_are_all_answered() {
    let out = output("foo & foo & foo & wait").await;
    assert_eq!(out.stdout, b"bar\nbar\nbar\n");
}

/// Delegated calls one after another inside a single command, which is the chain being a
/// loop rather than one extra round trip.
#[tokio::test]
async fn several_delegated_calls_run_in_one_execution() {
    let out = output("foo; report a; foo").await;
    assert_eq!(out.stdout, b"bar\nreport|a\nbar\n");
}

/// A name nobody registered is not on `PATH`, so the shell never reaches a shim at all.
#[tokio::test]
async fn an_unregistered_name_is_not_on_path() {
    let out = output("nosuchtool 2>/dev/null; echo code=$?").await;
    assert_eq!(out.stdout, b"code=127\n");
}

/// The whole of what makes `start` and `stop` optional: a command that finds a stopped
/// session gets one booted under it, and nothing had to ask.
///
/// `stop` takes the symlinks away, and the second command below — with no `start` in
/// front of it — finds them there again, in a directory built from the same `init`.
#[tokio::test]
async fn a_stopped_session_boots_again_for_the_next_command() {
    let mut fixture = Fixture::new().await;

    let out = fixture
        .console
        .exec(["sh", "-c", "command -v foo >/dev/null"], None)
        .await
        .unwrap();
    assert_eq!(out.code, 0, "foo should be on PATH");
    fixture.console.stop().await.unwrap();

    let out = fixture
        .console
        .exec(
            [
                "sh",
                "-c",
                "ls \"$(dirname \"$(command -v foo)\")\" | tr '\\n' ' '",
            ],
            None,
        )
        .await
        .unwrap();
    // A fresh directory, with exactly the names this session announced.
    let listed = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut names: Vec<&str> = listed.split_whitespace().collect();
    names.sort_unstable();
    assert_eq!(names, ["boom", "foo", "rawbytes", "report", "slow"]);
}

/// A client with nothing to delegate is still a client: no `start`, no executables, just
/// the one thing it came to ask for.
///
/// What it does not get is delegated names — it announced none — so `foo` is a command
/// that is not there, which is the same 127 any shell would report for a name it cannot
/// find. That it is the *fixture's* name makes the point: what puts one on `PATH` is
/// having said so, and nothing else.
#[tokio::test]
async fn a_command_runs_in_a_session_that_delegates_nothing() {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-local-console"));
    server.stderr(Stdio::inherit());
    let client = StdioClient::new(server).expect("starting the console server");

    let mut console = Console::builder()
        .client(client)
        .build()
        .await
        .expect("building the console");

    let out = console.exec(["sh", "-c", "echo hi"], None).await.unwrap();
    assert_eq!(out.stdout, b"hi\n");

    let out = console
        .exec(["sh", "-c", "foo 2>/dev/null; echo code=$?"], None)
        .await
        .unwrap();
    assert_eq!(out.stdout, b"code=127\n");
}

/// What `quit` is actually for: the server hears it, releases what it took, and exits —
/// rather than being killed with what it took still on disk.
///
/// A booted session is dropped without being stopped first, which is the whole point.
/// Nobody says `stop`, and the server still lets go of everything, because a server on its
/// way out does that on its own.
///
/// The socket is what proves it, and the scratch directory would not. Nothing ever sweeps
/// an abandoned socket — a server removes its own on the way out and no later run looks
/// for anyone else's — so a socket that disappears is a server that reached its own
/// `Drop`. The directory of symlinks is swept by the *next* server to boot, which in a
/// test binary running several at once would tidy up after a killed one and say nothing.
#[tokio::test]
async fn dropping_a_console_lets_the_server_clean_up_after_itself() {
    let mut fixture = Fixture::new().await;
    fixture.console.start().await.unwrap();

    // The server puts this in the environment of everything it runs, which is how a shim
    // finds its way home — and how this test finds the one thing only an ending removes.
    let out = fixture
        .console
        .exec(["sh", "-c", "echo $CORTEX_CONSOLE_SOCK"], None)
        .await
        .unwrap();
    let sock = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
    assert!(sock.exists(), "the server should have bound {sock:?}");

    drop(fixture);

    // The ending is a task, and the server has to hear it, stop reading and leave — so
    // this is waited for rather than looked at once.
    for _ in 0..200 {
        if !sock.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{sock:?} is still there, so the server was killed rather than asked to leave");
}

/// The point of the whole thing being async: two consoles, two server processes, two
/// commands that each pause on a delegated name — and the pair takes about as long as one
/// of them.
///
/// Each `slow` waits [`SLEEP`] inside *this* process while its command waits on the far
/// side of a pipe, so each console spends two of those waiting and the two consoles
/// together should still spend two. A client that ran them in turn, or that parked a
/// thread on either, would spend four.
///
/// The bound is three rather than four, which is loose enough not to mind a slow machine
/// and tight enough that nothing serial fits under it.
#[tokio::test]
async fn two_consoles_run_at_the_same_time() {
    let began = std::time::Instant::now();

    let one = tokio::spawn(async { Fixture::new().await.output("slow; slow").await });
    let two = tokio::spawn(async { Fixture::new().await.output("slow; slow").await });

    let (one, two) = tokio::join!(one, two);
    assert_eq!(one.unwrap().stdout, b"slept\nslept\n");
    assert_eq!(two.unwrap().stdout, b"slept\nslept\n");

    let took = began.elapsed();
    assert!(took < SLEEP * 3, "the two consoles took {took:?}, in turn");
}
