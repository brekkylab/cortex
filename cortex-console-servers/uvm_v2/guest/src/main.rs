//! `cortex-uvm-v2-guest` — the half that runs inside the guest.
//!
//! Built for the guest's target rather than this host's, which is why it is its own binary
//! and not a flag on the host's.
//!
//! libkrun execs one program as the guest's first userspace process, and this is it: it
//! builds the root the commands will run on, and then answers the host on the port it is
//! holding the other end of, for as long as the session lasts.
//!
//! A build step is the same thing with one command in it. It was briefly its own mode —
//! run once, leave a tar, exit — and that mode had no way to say what the command exited
//! with: the channel it did without was the thing that carried the answer.

mod boot;
mod init;
mod layer;
mod net;

// The cross-process contract, in the library all three halves of this take. Imported at
// the root so that the rest of the crate names it `crate::contract`, the way it would a
// module of its own.
pub(crate) use cortex_uvm_v2_common::contract;

use std::process::ExitCode;

/// A failure of ours, not a command's — the shell's code for "found it, could not run it".
const NOT_EXECUTABLE: u8 = 126;

fn main() -> ExitCode {
    // The overlay, the pseudo-filesystems, the shares, and the pivot onto them. What comes
    // back is what the base image states, which goes in front of every command.
    let image = match init::prepare() {
        Ok(image) => image,
        Err(e) => {
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            return ExitCode::from(NOT_EXECUTABLE);
        }
    };

    match boot::run(image) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            ExitCode::from(NOT_EXECUTABLE)
        }
    }
}
