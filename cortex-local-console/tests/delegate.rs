//! End-to-end over the real binary: a [`Console`] spawns it, it runs a command on the
//! host, and the delegated executables that command invokes come back here to be
//! answered.
//!
//! Which is the only way to test the interesting part. A delegated executable works only
//! if `execvp` finds a symlink, re-enters this binary as a shim, the shim dials the socket
//! the *server* bound, the server passes the call up the console channel as a `Delegated`,
//! this process answers it with a `resume`, and the bytes come back out of the shim's own
//! stdout — four processes and one channel. Nothing smaller than the whole thing exercises
//! it.

use std::process::{Command, Stdio};

use cortex::console::stdio::StdioClient;
use cortex::console::{Console, Exec, ExecResult};
use cortex::executable::{ExecCall, ExecResult as ExecOutput, Executable, ExecutableSet};

/// An executable with a canned answer — enough to tell a round trip from a coincidence.
struct Fixed {
    stdout: &'static [u8],
    stderr: &'static [u8],
    code: i32,
}

impl Executable for Fixed {
    fn exec(&self, _call: &ExecCall) -> ExecOutput {
        ExecOutput {
            stdout: self.stdout.to_vec(),
            stderr: self.stderr.to_vec(),
            exit_code: self.code,
            timed_out: false,
        }
    }
}

/// Reports everything it was told, to prove the whole call survives the trip.
struct Report;

impl Executable for Report {
    fn exec(&self, call: &ExecCall) -> ExecOutput {
        ExecOutput::ok(format!("{}|{}\n", call.name, call.args.join(",")))
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
    fn new() -> Fixture {
        let execs = ExecutableSet::new()
            .register(
                "foo",
                Fixed {
                    stdout: b"bar\n",
                    stderr: b"",
                    code: 0,
                },
            )
            .register(
                "boom",
                Fixed {
                    stdout: b"",
                    stderr: b"boom failed\n",
                    code: 3,
                },
            )
            .register(
                "rawbytes",
                Fixed {
                    // Not utf-8, and contains a NUL: nothing may touch it.
                    stdout: &[0xff, 0xfe, 0x00, b'\n'],
                    stderr: b"",
                    code: 0,
                },
            )
            .register("report", Report);

        // Only its stderr is ours to place — the client sets the two descriptors the
        // protocol runs on, and starts the process it owns from here on.
        let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-local-console"));
        server.stderr(Stdio::inherit());

        let console = Console::builder()
            .client(StdioClient::new(server).expect("spawning the console server"))
            .executables(execs)
            .default_timeout_ms(30_000)
            .build()
            .expect("building the console");

        Fixture { console }
    }

    /// Boot, run one command, and return what it produced.
    fn output(&mut self, script: &str) -> ExecResult {
        self.console.start().expect("booting the server");
        let result = self
            .console
            .exec(Exec {
                cmd: vec!["sh".into(), "-c".into(), script.into()],
                ..Exec::default()
            })
            .expect("running the command");
        self.console.stop().expect("stopping the server");
        result
    }
}

fn output(script: &str) -> ExecResult {
    Fixture::new().output(script)
}

/// The baseline: nothing delegated is involved, and a real command still runs.
#[test]
fn an_ordinary_command_runs_on_the_host() {
    let out = output("echo hello world");
    assert_eq!(out.stdout, b"hello world\n");
    assert_eq!(out.code, 0);
}

/// The command's own code comes back as the result's code, not as anything's failure.
#[test]
fn a_commands_exit_code_is_a_result_and_not_an_error() {
    let out = output("exit 7");
    assert_eq!(out.code, 7);
}

/// The whole point: `foo` is not a file anyone built, and running it produces what this
/// process said it produces.
#[test]
fn a_delegated_name_is_answered_by_this_process() {
    let out = output("foo");
    assert_eq!(out.stdout, b"bar\n");
    assert_eq!(out.code, 0);
}

/// A delegated executable is a program as far as the shell is concerned, so it composes
/// like one.
#[test]
fn a_delegated_name_pipes_like_a_program() {
    let out = output("foo | tr a-z A-Z");
    assert_eq!(out.stdout, b"BAR\n");
}

/// Its stderr and its code arrive too, and on the right streams.
#[test]
fn a_delegated_name_reports_stderr_and_its_code() {
    let out = output("boom; echo code=$?");
    assert_eq!(out.stdout, b"code=3\n");
    assert_eq!(out.stderr, b"boom failed\n");
}

/// Nothing on the way through is text: the bytes come back exactly as they were written,
/// through JSON, a socket, a pipe and a shell.
#[test]
fn a_delegated_names_output_is_bytes() {
    let out = output("rawbytes");
    assert_eq!(out.stdout, [0xff, 0xfe, 0x00, b'\n']);
}

/// The name it was called by and its arguments survive the trip.
#[test]
fn the_call_arrives_as_it_was_made() {
    let out = output("report one two");
    assert_eq!(out.stdout, b"report|one,two\n");
}

/// Several started together and served one at a time: each shim waits its turn on the
/// server's queue, and all three get their answer.
///
/// Which is the whole of what serialised delegation gives up — the three run in sequence
/// rather than at once — and the whole of what it does not: a delegated call carries no
/// input and so waits on nothing but being served.
#[test]
fn delegated_names_started_together_are_all_answered() {
    let out = output("foo & foo & foo & wait");
    assert_eq!(out.stdout, b"bar\nbar\nbar\n");
}

/// Delegated calls one after another inside a single command, which is the chain being a
/// loop rather than one extra round trip.
#[test]
fn several_delegated_calls_run_in_one_execution() {
    let out = output("foo; report a; foo");
    assert_eq!(out.stdout, b"bar\nreport|a\nbar\n");
}

/// A name nobody registered is not on `PATH`, so the shell never reaches a shim at all.
#[test]
fn an_unregistered_name_is_not_on_path() {
    let out = output("nosuchtool 2>/dev/null; echo code=$?");
    assert_eq!(out.stdout, b"code=127\n");
}

/// `stop` takes the symlinks away and another `start` puts them back, because releasing
/// what booting took is not the end of the session.
#[test]
fn a_console_can_be_stopped_and_started_again() {
    let mut fixture = Fixture::new();

    fixture.console.start().unwrap();
    let out = fixture
        .console
        .exec(Exec {
            cmd: vec!["sh".into(), "-c".into(), "command -v foo >/dev/null".into()],
            ..Exec::default()
        })
        .unwrap();
    assert_eq!(out.code, 0, "foo should be on PATH while started");
    fixture.console.stop().unwrap();

    fixture.console.start().unwrap();
    let out = fixture
        .console
        .exec(Exec {
            cmd: vec![
                "sh".into(),
                "-c".into(),
                "ls \"$(dirname \"$(command -v foo)\")\" | tr '\\n' ' '".into(),
            ],
            ..Exec::default()
        })
        .unwrap();
    // A fresh directory, with exactly the names this session announced.
    let listed = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut names: Vec<&str> = listed.split_whitespace().collect();
    names.sort_unstable();
    assert_eq!(names, ["boom", "foo", "rawbytes", "report"]);
    fixture.console.stop().unwrap();
}

/// A server told the session is over exits, and `shutdown` is where that is waited for:
/// the process is the client's, so a clean ending is what comes back here.
#[test]
fn shutting_down_collects_the_process() {
    let mut fixture = Fixture::new();
    fixture.console.start().unwrap();

    fixture
        .console
        .shutdown()
        .expect("the server should end cleanly");
}
