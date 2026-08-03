//! The role this binary plays when it is reached through one of its own
//! symlinks.
//!
//! It runs as an ordinary process, in whatever context called it — a pipeline,
//! a script, a `subprocess.run`. Its whole job is to be transparent: report the
//! call, replay the answer on its own stdout/stderr, and exit with the code the
//! executable returned. Nothing about it should be visible to the caller.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use crate::ipc::{Call, CallReply, SOCK_ENV};

/// A failure that is ours, not the executable's. 126 is what a shell reports
/// for a command it found but could not run — which is exactly what happened.
const NOT_EXECUTABLE: u8 = 126;

/// Forward a call named `tool` to the server and become its result.
pub fn run(tool: &str) -> ExitCode {
    match forward(tool) {
        Ok(reply) => {
            // Straight to our own descriptors: they are the caller's pipe, file
            // or terminal, and the point of the shim is that the output arrives
            // there the same way a real program's would.
            let _ = std::io::stdout().write_all(reply.stdout.as_bytes());
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().write_all(reply.stderr.as_bytes());
            // A process can only exit 0..=255; a negative or oversized code
            // cannot be represented, so clamp rather than silently truncate.
            ExitCode::from(reply.code.clamp(0, 255) as u8)
        }
        Err(e) => {
            eprintln!("{tool}: {e}");
            ExitCode::from(NOT_EXECUTABLE)
        }
    }
}

/// One request, one reply, one connection.
fn forward(tool: &str) -> anyhow::Result<CallReply> {
    let sock = std::env::var(SOCK_ENV)
        .map_err(|_| anyhow::anyhow!("{SOCK_ENV} is unset — not running under a console server"))?;

    let call = Call {
        name: tool.to_string(),
        args: std::env::args().skip(1).collect(),
        cwd: std::env::current_dir()?,
    };

    let stream = UnixStream::connect(&sock)?;
    let mut writer = &stream;
    serde_json::to_writer(&mut writer, &call)?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    // The server answers exactly one line and we read exactly one, so the
    // connection carries no state worth keeping — it closes with this scope.
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    if line.is_empty() {
        anyhow::bail!("console server closed the connection without replying");
    }
    Ok(serde_json::from_str(&line)?)
}
