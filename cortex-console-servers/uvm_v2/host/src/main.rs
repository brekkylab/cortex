//! `cortex-uvm-v2-host` — the half that answers a console session.
//!
//! Reads requests on stdin and writes answers on stdout, per [`cortex::console`]; what it
//! does with one is to run it inside a micro-VM. The VM is not in this process: a session
//! spawns `cortex-uvm-v2-boot`, the signed binary sitting beside this one, and talks to the
//! guest down a socket that boot turns into a console port. See `session::halves` for why
//! the hypervisor is somewhere else and how the three files find each other.
//!
//! # Two things to be, and a session is the default
//!
//! `run` answers a console session; `rootfs` tends the images a session boots from and exits.
//! A client spawns this binary to be its server and passes nothing, which is why no arguments
//! means `run` rather than a usage message: the common case is the one a caller does not have
//! to spell.

mod rootfs;
mod session;

// The cross-process contract, in the library all three halves of this take. Imported at
// the root so that the rest of the crate names it `crate::contract`, the way it would a
// module of its own.
pub(crate) use cortex_uvm_v2_common::contract;

use std::path::PathBuf;
use std::process::ExitCode;

/// A failure of ours, not a command's — the shell's code for "found it, could not run it".
const NOT_EXECUTABLE: u8 = 126;

/// Everything this server keeps on disk, under one root — `$CORTEX_UVM_HOME`, or
/// `$HOME/.cache/cortex`.
///
/// ```text
/// {home}/
/// ├── rootfs/                 the image store: shared, content-addressed, kept
/// │   ├── layers/             one EROFS per layer, named by its diff_id
/// │   ├── fsmeta/  vmdk/      merged metadata, and the descriptor naming a stack
/// │   ├── images/             what this end states: a base with steps over it
/// │   └── runs/               what a RUN left, under the image and command asked for
/// └── session/                one directory per live server process
///     └── {pid}/
///         ├── session.ext4    what the session wrote — outlives any one machine
///         └── machine/        what only makes sense while one is up
///             ├── boot/       the root libkrun hands over: guest binary, image spec, and
///             │               the layer the guest leaves there for this end to read
///             ├── port.sock   the console channel
///             └── console.log the kernel's own output
/// ```
///
/// Split by lifetime rather than by who wrote it, which is what makes each level's cleanup
/// one call: a `stop` removes `machine/`, the end of a session removes `{pid}/`, and nothing
/// removes `rootfs/` on its own, because a rebuild is what it exists to make cheap — only
/// `rootfs remove` does, for an image somebody said they are done with. One variable moves all
/// of it, which is what a test that wants none of it near a real cache needs.
pub(crate) fn home() -> PathBuf {
    std::env::var_os("CORTEX_UVM_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/cortex")))
        .expect("neither CORTEX_UVM_HOME nor HOME is set")
}

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
