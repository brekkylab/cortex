//! `cortex-local-console-server` — entry point for the host-local backend.
//!
//! Speaks an MCP-style stdio protocol: boot, then serve JSON requests off stdin
//! one at a time until the caller sends `quit` or closes the pipe. The wire and
//! the loop both live in [`cortex::console`], so every console-server backend
//! behaves identically on the parts that are not about running the command.
//!
//! Serving means running the request's argv here on the host and reporting what
//! it did. The command's output is captured rather than inherited, because it
//! has to come back as fields of a JSON object — which also means our own exit
//! code says nothing about the command's: that is `code` in the response.
//!
//! Every request gets exactly one response, whether the command ran or not.
//! Failures we hit ourselves — an empty argv, a program that isn't there — are
//! reported the way a shell reports them: the reason in `stderr`, 127 or 126 in
//! `code`. So the caller has one thing to parse and one place to look, instead
//! of a second error channel that only sometimes carries anything.
//!
//! Unix-only: `code` encodes a signal death as `128 + signal`, which needs the
//! signal number from the Unix `ExitStatus` extension.
//!
//! # Two roles, one binary
//!
//! Boot also puts the [`ExecutableSet`](cortex::ExecutableSet)'s virtual
//! executables on `PATH`, as symlinks back to this same binary (see
//! [`bin_dir`]). So this program is reached two ways, and [`role`] tells them
//! apart by the name it was invoked under:
//!
//! - under its own name — **server**: serve the stdio loop, above.
//! - under any other — **shim**: that name is a registered executable someone
//!   just ran, so report the call over the socket and become its result (see
//!   [`shim`]).
//!
//! There is nothing to build or ship for the second role. A virtual executable
//! costs one `ExecutableSet::register` line and one `symlink(2)` at boot.
//!
//! ```text
//! $ cargo run -q -p cortex-local-console-server
//! {"type":"exec","cmd":["echo","hello world"]}
//! {"type":"exec","stdout":"hello world\n","stderr":"","code":0}
//!
//! # `foo` is not a file we built — it is a symlink to this binary, and its
//! # output came from Rust running in the server process.
//! {"type":"exec","cmd":["foo"]}
//! {"type":"exec","stdout":"bar\n","stderr":"","code":0}
//!
//! # Which means it is an ordinary command wherever commands are ordinary.
//! {"type":"exec","cmd":["sh","-c","foo | tr a-z A-Z; command -v foo >/dev/null && echo found"]}
//! {"type":"exec","stdout":"BAR\nfound\n","stderr":"","code":0}
//!
//! # Nothing is wrapped around the argv, so a shell is something the caller
//! # asks for — and its exit code comes back as `code`, not as ours.
//! {"type":"exec","cmd":["sh","-c","echo out; echo err >&2; exit 42"]}
//! {"type":"exec","stdout":"out\n","stderr":"err\n","code":42}
//!
//! {"type":"quit"}
//! $
//! ```

mod bin_dir;
mod callback;
mod demo;
mod ipc;
mod shim;

use std::ffi::OsStr;
use std::io;
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::thread;

use cortex::console::{ExecRequest, ExecResponse, Request, Response, enter_loop};

use bin_dir::BinDir;
use ipc::SOCK_ENV;

/// Codes for a command that never ran, borrowed from the shell so they mean
/// what a caller already expects: 127 for a program that could not be found,
/// 126 for one that could not be executed.
///
/// Both are values a real command could return for its own reasons, so neither
/// proves the failure was ours. Nothing could — every code in 0..=255 is
/// reachable by an ordinary `exit()`. `stderr` carries the actual reason, and
/// when the failure is ours it is the only thing in there.
const NOT_EXECUTABLE: i32 = 126;
const NOT_FOUND: i32 = 127;

/// Which of the two jobs this process was started to do.
enum Role {
    /// Serve the stdio loop.
    Server,
    /// Stand in for the named virtual executable.
    Shim(String),
}

/// Decide the role from `argv[0]`.
///
/// `execvp` puts the path it resolved into `argv[0]`, so a call through one of
/// our symlinks arrives carrying the link's name — which is the executable's
/// name, and the only thing distinguishing that invocation from a normal one.
/// Comparing against `CARGO_BIN_NAME` rather than a literal keeps this correct
/// if the crate is renamed again.
fn role() -> Role {
    let argv0 = std::env::args().next().unwrap_or_default();
    match Path::new(&argv0).file_name().and_then(OsStr::to_str) {
        Some(name) if name != env!("CARGO_BIN_NAME") => Role::Shim(name.to_string()),
        // No `argv[0]` at all: nothing claims we are a shim, so serve.
        _ => Role::Server,
    }
}

fn main() -> ExitCode {
    match role() {
        Role::Shim(name) => shim::run(&name),
        Role::Server => match server() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("cortex-local-console-server: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Boot, then serve until told to stop.
fn server() -> anyhow::Result<()> {
    let execs = Arc::new(demo::set());

    // Order matters: the names have to be linked and the socket bound before
    // anything can be spawned that might call one.
    let dir = BinDir::create(execs.names())?;
    let listener = UnixListener::bind(dir.socket())?;

    // SAFETY: this is the only thread — the listener below is the first one we
    // spawn — so nothing can be reading the environment concurrently.
    unsafe {
        // Appended, not prepended: these names are meant to add commands, not
        // to quietly shadow a real `git` or `python` a caller meant to run.
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{path}:{}", dir.bin().display()));
        // Set process-wide rather than per-child, so it reaches grandchildren:
        // a shim is usually spawned by something we spawned, not by us.
        std::env::set_var(SOCK_ENV, dir.socket());
    }

    thread::spawn({
        let execs = Arc::clone(&execs);
        move || callback::serve(listener, execs)
    });

    enter_loop(
        |req| match req {
            Request::Exec(cmd) => Response::Exec(run(&cmd)),
            // `enter_loop` returns on `Quit` instead of dispatching it.
            Request::Quit => unreachable!("the loop handles Quit itself"),
        },
        // stderr, not stdout: stdout is the protocol's channel and a caller is
        // parsing it a line at a time.
        //
        // `dir` is dropped on the way out of this function rather than here,
        // which also covers a panic unwinding through the loop. Either way the
        // symlinks and the socket go with it.
        || eprintln!("bye"),
    )
}

/// Run the request's argv and report what it did.
fn run(req: &ExecRequest) -> ExecResponse {
    let Some((program, args)) = req.cmd.split_first() else {
        return refused(NOT_EXECUTABLE, "empty command".into());
    };

    let out = match Command::new(program).args(args).output() {
        Ok(out) => out,
        Err(e) => {
            let code = match e.kind() {
                io::ErrorKind::NotFound => NOT_FOUND,
                _ => NOT_EXECUTABLE,
            };
            return refused(code, format!("{program}: {e}"));
        }
    };

    ExecResponse {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        // `code()` is `None` exactly when a signal killed the child, in which
        // case `signal()` is `Some`; the fallback is unreachable in practice.
        code: out
            .status
            .code()
            .or_else(|| out.status.signal().map(|s| 128 + s))
            .unwrap_or(NOT_EXECUTABLE),
    }
}

/// A response for a command that never ran.
///
/// The reason is prefixed, because `stderr` is otherwise the command's own
/// voice — a caller showing it to a human should not have our words read as
/// theirs.
fn refused(code: i32, reason: String) -> ExecResponse {
    ExecResponse {
        stdout: String::new(),
        stderr: format!("cortex-local-console-server: {reason}\n"),
        code,
    }
}
