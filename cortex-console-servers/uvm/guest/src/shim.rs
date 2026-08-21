//! The role this binary plays when it is reached through one of its own symlinks.
//!
//! It runs as an ordinary guest process, in whatever context called it — a pipeline, a
//! script, a `subprocess.run`. Its whole job is to be transparent: report the call, put
//! back what comes back on its own stdout and stderr, and exit with the code the
//! executable returned. Nothing about it should be visible to the caller, and nothing
//! about it should suggest that the thing it stands in for ran on the other side of a
//! hypervisor.
//!
//! One connection, one call, one answer — which is the console protocol's shape too, so
//! this speaks it rather than a wire of its own.

use std::process::ExitCode;

use anyhow::bail;
use cortex::console::stdio::{read, write};
use cortex::console::{Call, Exec, ExecCmd, ExecResult, Message};
use tokio::io::AsyncWriteExt as _;
use tokio::net::UnixStream;

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
pub async fn run(tool: &str) -> ExitCode {
    match forward(tool).await {
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
///
/// The stream is written and then read through the same borrow, which is all this needs:
/// the answer cannot arrive before the question goes out, so there is nothing here for
/// two halves to do at once.
async fn forward(tool: &str) -> anyhow::Result<i32> {
    let sock = std::env::var(SOCK_ENV)
        .map_err(|_| anyhow::anyhow!("{SOCK_ENV} is unset — not running under a console"))?;

    let mut stream = UnixStream::connect(&sock).await?;

    // The name is the first word of the command, which is how any `exec` names what to
    // run — a delegated one is not a different kind of request, only one whose program
    // lives elsewhere.
    let mut cmd = vec![tool.to_string()];
    cmd.extend(std::env::args().skip(1));

    write(
        &mut stream,
        &Message::Request {
            id: CALL_ID,
            call: Call::Exec(Exec {
                cmd: ExecCmd::New(cmd),
                // Absolute, in the guest's filesystem — which is the host's spelling too,
                // since the tree is mounted at the path the host names it by. So nothing
                // downstream translates this; it is already a path the client can use.
                //
                // `to_str` and not `to_string_lossy`, and this is the last place the bytes
                // exist: lossy would substitute U+FFFD and hand on a `String` that looks
                // valid and names something else, which nothing downstream could detect.
                cwd: std::env::current_dir()
                    .ok()
                    .and_then(|p| p.to_str().map(str::to_owned)),
                // All of it, so a delegated name sees what a program on `PATH` would.
                // `vars_os` and not `vars`: the latter panics on an entry that is not
                // UTF-8, and one variable nothing can spell is no reason to fail a call —
                // it is left out, the way a `cwd` with no `String` form is.
                env: std::env::vars_os()
                    .filter_map(|(name, value)| {
                        Some((name.into_string().ok()?, value.into_string().ok()?))
                    })
                    .collect(),
                ..Exec::default()
            }),
        },
    )
    .await?;

    let Some(message) = read(&mut stream).await? else {
        bail!("the console closed the connection without answering");
    };
    let Message::Response { outcome, .. } = message else {
        bail!("the console sent {message:?} instead of an answer");
    };

    // Everything the executable produced arrives at once — this protocol has no output
    // chunks — so what makes the shim transparent is *where* the bytes go, not when.
    let result: ExecResult = outcome.take().map_err(|e| anyhow::anyhow!("{e}"))?;

    // Our own descriptors, and they are whatever ran us handed down — a pipeline, a file,
    // the guest console. Flushed before the code goes back, because exiting is what
    // happens next and a buffer nobody drained would be output the caller never saw.
    let mut stdout = tokio::io::stdout();
    stdout.write_all(&result.stdout).await?;
    stdout.flush().await?;

    let mut stderr = tokio::io::stderr();
    stderr.write_all(&result.stderr).await?;
    stderr.flush().await?;

    Ok(result.code)
}
