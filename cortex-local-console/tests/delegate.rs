//! End-to-end over the real binary: a [`Console`] spawns it, it runs a command on the
//! host, and the delegated executables that command invokes come back here to be
//! answered.
//!
//! Which is the only way to test the interesting part. A delegated executable works only
//! if `execvp` finds a symlink, re-enters this binary as a shim, the shim dials the socket
//! *this* process bound, and the answer comes back out of the shim's own stdout — four
//! processes and two channels. Nothing smaller than the whole thing exercises it.

use std::io;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use cortex::console::stdio::{StdioRequester, StdioResponder};
use cortex::console::{Console, Exec, ExecResult, Responsable};
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

/// A console over the real binary, plus the socket its shims dial.
///
/// Bound under `/tmp` rather than `$TMPDIR`, because `sockaddr_un.sun_path` is 104 bytes
/// on macOS and the per-user temp directory is most of that on its own.
struct Fixture {
    console: Console,
    sock: PathBuf,
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

        // One per test, and tests share a process — so the pid alone is not unique.
        let sock = PathBuf::from(format!(
            "/tmp/cx-{}-{:p}.sock",
            std::process::id(),
            &execs as *const _
        ));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("binding the delegate socket");

        let mut child = Command::new(env!("CARGO_BIN_EXE_cortex-local-console"))
            // The way home for every shim, inherited by everything the server spawns.
            .env("CORTEX_CONSOLE_SOCK", &sock)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawning the console server");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let console = Console::builder()
            .client(StdioRequester::new(stdout, stdin))
            .server(child)
            // One connection is one delegated call, so accepting is what waits for the
            // next one.
            .delegates(move || {
                let (stream, _) = listener.accept()?;
                let server = StdioResponder::new(stream.try_clone()?, stream);
                Ok(Some(Box::new(server) as Box<dyn Responsable + Send>))
            })
            .executables(execs)
            .default_timeout_ms(30_000)
            .build()
            .expect("building the console");

        Fixture { console, sock }
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

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock);
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

/// Several at once, which is what the thread per connection is for: each blocks until
/// this process answers it, so serving them in turn would deadlock a pipeline.
#[test]
fn delegated_names_can_be_in_flight_together() {
    let out = output("foo & foo & foo & wait");
    assert_eq!(out.stdout, b"bar\nbar\nbar\n");
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

/// A server whose stdin ends exits, and `shutdown` collects it.
#[test]
fn shutting_down_collects_the_process() {
    let mut fixture = Fixture::new();
    fixture.console.start().unwrap();

    let status = fixture.console.shutdown().unwrap();
    let status = status.expect("a console given a process reports its status");
    assert!(status.success(), "{status:?}");
}

/// Only so `io` is what the accept closure's signature says it is.
const _: fn(io::Error) = |_| ();
