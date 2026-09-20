//! `cortex-uvm-v2-host` — the half that answers a console session.
//!
//! Reads requests on stdin and writes answers on stdout, per [`cortex::console`]; what it
//! does with one is to run it inside a micro-VM. The VM is not in this process: a session
//! spawns `cortex-uvm-v2-boot`, a signed copy of a binary this one carries inside it, and
//! talks to the guest down a socket that boot turns into a console port. See
//! `session::helper` for why the hypervisor is somewhere else.
//!
//! # Two things to be, and a session is the default
//!
//! `run` answers a console session; `rootfs` tends the images a session boots from and exits.
//! A client spawns this binary to be its server and passes nothing, which is why no arguments
//! means `run` rather than a usage message: the common case is the one a caller does not have
//! to spell.

mod contract;
mod rootfs;
mod session;

use std::process::ExitCode;

/// A failure of ours, not a command's — the shell's code for "found it, could not run it".
const NOT_EXECUTABLE: u8 = 126;

#[tokio::main]
async fn main() -> ExitCode {
    match run(std::env::args().skip(1)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            ExitCode::from(NOT_EXECUTABLE)
        }
    }
}

async fn run(argv: impl IntoIterator<Item = impl AsRef<str>>) -> anyhow::Result<()> {
    let mut argv = argv.into_iter();
    let mode = argv.next();
    match mode.as_ref().map(|m| m.as_ref()) {
        Some("rootfs") => rootfs::run(argv).await,
        None | Some("run") => session::run().await,
        Some(other) => anyhow::bail!("{other:?} is not `rootfs` or `run`"),
    }
}
