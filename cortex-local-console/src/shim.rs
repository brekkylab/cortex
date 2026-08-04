//! The role this binary plays when it is reached through one of its own symlinks.
//!
//! It runs as an ordinary process, in whatever context called it — a pipeline, a script,
//! a `subprocess.run`. Its whole job is to be transparent: report the call, put back what
//! comes back on its own stdout and stderr, and exit with the code the executable
//! returned. Nothing about it should be visible to the caller.
//!
//! One connection, one call, one answer — which is the console protocol's shape too, so
//! this speaks it rather than a wire of its own.

use std::io::{self, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use anyhow::bail;
use cortex::console::stdio::{read, write};
use cortex::console::{Call, Exec, ExecResult, Message};

use crate::ipc::SOCK_ENV;

/// A failure that is ours, not the executable's. 126 is what a shell reports for a
/// command it found but could not run — which is exactly what happened.
const NOT_EXECUTABLE: u8 = 126;

/// The id on a shim's one request.
///
/// A connection carries a single call, so there is nothing for an id to tell apart and
/// nothing to allocate. It is on the wire because a request without one would be a
/// notification, and this end is waiting for an answer.
const CALL_ID: u64 = 0;

/// Forward a call named `tool` to the client and become its result.
pub fn run(tool: &str) -> ExitCode {
    match forward(tool) {
        // A process can only exit 0..=255; a negative or oversized code cannot be
        // represented, so clamp rather than silently truncate.
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("{tool}: {e}");
            ExitCode::from(NOT_EXECUTABLE)
        }
    }
}

/// One call, one connection: send it, put the answer back, return the code.
fn forward(tool: &str) -> anyhow::Result<i32> {
    let sock = std::env::var(SOCK_ENV)
        .map_err(|_| anyhow::anyhow!("{SOCK_ENV} is unset — not running under a console"))?;

    let stream = UnixStream::connect(&sock)?;

    // The name is `cmd[0]`, which is how any `exec` names what to run — a delegated one
    // is not a different kind of request, only one whose program lives elsewhere.
    let mut cmd = vec![tool.to_string()];
    cmd.extend(std::env::args().skip(1));

    write(
        &mut &stream,
        &Message::Request {
            id: CALL_ID,
            call: Call::Exec(Exec {
                cmd,
                ..Exec::default()
            }),
        },
    )?;

    let mut reader = BufReader::new(&stream);
    let Some(message) = read(&mut reader)? else {
        bail!("the console closed the connection without answering");
    };
    let Message::Response { outcome, .. } = message else {
        bail!("the console sent {message:?} instead of an answer");
    };

    // Everything the executable produced arrives at once — this protocol has no output
    // chunks — so what makes the shim transparent is *where* the bytes go, not when.
    let result: ExecResult = outcome.take().map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut stdout = io::stdout().lock();
    stdout.write_all(&result.stdout)?;
    stdout.flush()?;

    let mut stderr = io::stderr().lock();
    stderr.write_all(&result.stderr)?;
    stderr.flush()?;

    Ok(result.code)
}
