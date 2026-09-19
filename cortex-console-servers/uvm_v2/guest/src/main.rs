//! `cortex-uvm-v2-guest` — the half that runs inside the guest.
//!
//! Built for the guest's target rather than this host's, which is why it is its own binary
//! and not a flag on the host's.
//!
//! # One binary, two modes
//!
//! libkrun execs one program as the guest's first userspace process, and this is it. What
//! that program does afterwards is the difference between the two things a micro-VM is
//! started for here, so it is an argument rather than a second binary — the root each one
//! has to build is the same work, and building it twice is two things to keep in step.
//!
//! * `boot` — build the root, then stay: answer commands on the port the host holds the
//!   other end of, for as long as the session lasts.
//! * `exec <argv>...` — build the root, run one command, leave what it wrote behind, exit
//!   with its status. No port, no protocol: a build step wants one answer and no session.

mod contract;
mod init;
mod net;

use std::process::ExitCode;

/// A failure of ours, not a command's — the shell's code for "found it, could not run it".
const NOT_EXECUTABLE: u8 = 126;

fn main() -> ExitCode {
    let mut argv = std::env::args().skip(1);
    let mode = argv.next();

    // The root both modes need, before either of them can be told apart: the overlay, the
    // pseudo-filesystems, the shares, and the pivot onto them. What comes back is what the
    // base image states, which both modes put in front of a command.
    let image = match init::prepare() {
        Ok(image) => image,
        Err(e) => {
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            return ExitCode::from(NOT_EXECUTABLE);
        }
    };

    let outcome = match mode.as_deref() {
        Some("boot") => agent(image),
        Some("exec") => return exec(image, argv),
        None => Err(anyhow::anyhow!(
            "no mode: this is started as `boot` or `exec <argv>...`"
        )),
        Some(other) => Err(anyhow::anyhow!("{other:?} is not `boot` or `exec`")),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            ExitCode::from(NOT_EXECUTABLE)
        }
    }
}

/// Answer the host on the port it is holding, until the session ends.
fn agent(image: contract::ImageSpec) -> anyhow::Result<()> {
    let port = init::open_port()?;
    todo!("the wire on {port:?}, a child process per command, stating {:?}", image.env)
}

/// Run one command and exit with what it exited with.
fn exec(image: contract::ImageSpec, argv: impl Iterator<Item = String>) -> ExitCode {
    todo!(
        "{:?} stating {:?}, and what it wrote left where a commit can find it",
        argv.collect::<Vec<_>>(),
        image.env
    )
}
