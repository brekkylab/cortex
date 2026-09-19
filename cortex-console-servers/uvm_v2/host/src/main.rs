//! `cortex-uvm-v2-host` — the half that answers a console session.
//!
//! Reads requests on stdin and writes answers on stdout, per [`cortex::console`]; what it
//! does with one is to run it inside a micro-VM this process brings up. None of that is
//! here yet — so far this owns the wire and reclaims what an earlier run abandoned.
//!
//! # Two things to be, and a session is the default
//!
//! `run` answers a console session; `rootfs` tends the images a session boots from and exits.
//! A client spawns this binary to be its server and passes nothing, which is why no arguments
//! means `run` rather than a usage message: the common case is the one a caller does not have
//! to spell.
//!
//! There is a third, and **nobody calls it.** `boot` is this binary holding a hypervisor, and
//! it is reached only by a session spawning a signed copy of itself — the entitlements a VM
//! needs are read at `exec`, so the process that creates one can never be the process that
//! decided to. It is left out of the refusal below for that reason: a mode a caller cannot
//! usefully type is not one to offer them.

mod contract;
mod rootfs;
mod session;

use std::process::ExitCode;

/// A failure of ours, not a command's — the shell's code for "found it, could not run it".
const NOT_EXECUTABLE: u8 = 126;

fn main() -> ExitCode {
    // Before any runtime of ours. The VMM drives one of its own and blocks on it, which is
    // what tokio refuses to let a thread already inside a runtime do — so the mode that
    // holds a hypervisor cannot be started from within one.
    let outcome = if std::env::args().nth(1).as_deref() == Some("boot") {
        session::boot(std::env::args_os().skip(2))
    } else {
        match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(runtime) => runtime.block_on(run(std::env::args().skip(1))),
            Err(e) => Err(anyhow::anyhow!("building a runtime: {e}")),
        }
    };

    match outcome {
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
