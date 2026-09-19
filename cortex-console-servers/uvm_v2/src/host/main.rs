//! `cortex-uvm-v2-host` — the half that answers a console session.
//!
//! Reads requests on stdin and writes answers on stdout, per [`cortex::console`]; what it
//! does with one is to run it inside a micro-VM this process brings up. None of that is
//! here yet — so far this owns the wire and nothing more.

use cortex::console::stdio::StdioServer;

/// A failure of ours, not a command's — the shell's code for "found it, could not run it".
const NOT_EXECUTABLE: u8 = 126;

/// Multi-threaded, which is `#[tokio::main]`'s default and the reason it is left at it: a
/// relay's waiting overlaps. The channel to the client, the channel into the guest, a boot
/// child being reaped, and the blocking work of building an image are all outstanding at
/// once, and a current-thread runtime would serialize them behind whichever blocked first.
#[tokio::main]
async fn main() -> std::process::ExitCode {
    // Each command's exit code travels back inside its own answer, so this one says only
    // whether the session itself worked.
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            std::process::ExitCode::from(NOT_EXECUTABLE)
        }
    }
}

async fn run() -> anyhow::Result<()> {
    // Taken for the life of the process, and taken first: from here on stdout carries
    // frames and nothing else, which is why every diagnostic above and below goes to
    // stderr. Nothing enforces that — it is a rule, and this is where it starts applying.
    let _server = StdioServer::stdio()?;

    Ok(())
}
