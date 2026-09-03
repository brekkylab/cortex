//! `cortex-uvm-guest` — the half of the micro-VM console that lives inside the guest.
//!
//! [`cortex-uvm-console`] boots a micro-VM and forwards the console session into it. This
//! is what the session arrives at: a program libkrun execs as the guest's first
//! userspace process, which builds the root the commands will run on, opens the virtio
//! port the host is holding the other end of, and from there answers `init`, `exec`,
//! `read` and `write` exactly the way a console server on a host would.
//!
//! It never sees stdin or stdout. Those belong to the guest's own console — kernel
//! messages, a panic, whatever a command inherits — so the protocol runs over a named
//! virtio-console port instead, and stays out of the way of everything a person would
//! want to read while debugging a boot.
//!
//! # Two jobs, one process
//!
//! - **init** — mount the overlay root, the pseudo-filesystems and the workspace share,
//!   before there is a root worth running anything on. All of it is `mount(2)`, because
//!   a guest image's own `mount` cannot be relied on: util-linux drives mounts through
//!   the `fsopen`/`fsmount` API the libkrun kernel rejects, where the syscall itself
//!   works. See [`init`].
//! - **agent** — answer the session on the virtio port. Same job as
//!   [`cortex-local-console`]'s server, on the other side of a hypervisor. See [`agent`].
//!
//! The two are one process: init does its work in `main` and then *becomes* the agent,
//! rather than exec'ing it. That is not a shortcut but the only thing that works — after
//! the `pivot_root` in [`init::mount_root`] the binary's own file is no longer reachable
//! by any path, and a process already loaded does not care.
//!
//! [`cortex-uvm-console`]: https://docs.rs/cortex-uvm-console
//! [`cortex-local-console`]: https://docs.rs/cortex-local-console

mod agent;
mod contract;
mod init;
mod net;

use std::process::ExitCode;

/// A failure of ours, not the command's — the shell's code for "found it, could not run
/// it".
const NOT_EXECUTABLE: u8 = 126;

/// The agent waits on things it does not drive — a port, a command — so it is async. The
/// runtime is built by hand rather than by `#[tokio::main]` because the init work above it
/// is not: `pivot_root` moves the root out from under every thread in the process, and it is
/// easier to reason about when there is only one.
fn main() -> ExitCode {
    let (port, image) = match init::prepare() {
        Ok(prepared) => prepared,
        Err(e) => {
            eprintln!("cortex-uvm-guest: {e}");
            return ExitCode::from(NOT_EXECUTABLE);
        }
    };
    // A session's own answers travel inside its responses, so this code says only whether
    // the session itself worked.
    match runtime().block_on(agent::run(port, image)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cortex-uvm-guest: {e}");
            ExitCode::from(NOT_EXECUTABLE)
        }
    }
}

/// Multi-threaded, because the agent's work is: a command's two pipes drain while the agent
/// waits for it to end, and nothing about a session says how many threads the programs it
/// spawns will want out of the runtime they were spawned from.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime")
}
