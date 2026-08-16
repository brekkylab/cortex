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
//! # Three jobs, one binary
//!
//! - **init** — mount the overlay root, the pseudo-filesystems and the workspace share,
//!   before there is a root worth running anything on. All of it is `mount(2)`, because
//!   a guest image's own `mount` cannot be relied on: util-linux drives mounts through
//!   the `fsopen`/`fsmount` API the libkrun kernel rejects, where the syscall itself
//!   works. See [`init`].
//! - **agent** — answer the session on the virtio port. Same job as
//!   [`cortex-local-console`]'s server role, on the other side of a hypervisor. See
//!   [`agent`].
//! - **shim** — stand in for a delegated executable a command just ran, and report the
//!   call back to the agent over a guest-local socket. See [`shim`].
//!
//! The first two are one process: init does its work in `main` and then *becomes* the
//! agent, rather than exec'ing it. That is not a shortcut but the only thing that works
//! — after the `pivot_root` in [`init::mount_root`] the binary's own file is no longer
//! reachable by any path, and a process already loaded does not care. What it does mean
//! is that the shims need a file to point at, which is why init copies itself onto the
//! new root on the way past.
//!
//! [`cortex-uvm-console`]: https://docs.rs/cortex-uvm-console
//! [`cortex-local-console`]: https://docs.rs/cortex-local-console

mod agent;
mod contract;
mod init;
mod ipc;
mod shim;

use std::path::Path;
use std::process::ExitCode;

/// A failure of ours, not the command's — the shell's code for "found it, could not run
/// it".
const NOT_EXECUTABLE: u8 = 126;

/// Which of the three jobs this process was started to do.
enum Role {
    /// Build the root, then answer the session on the virtio port.
    Agent,
    /// Stand in for the named delegated executable.
    Shim(String),
}

/// Decide the role from `argv[0]`.
///
/// `execvp` puts the path it resolved into `argv[0]`, so a call through one of the
/// symlinks in [`agent::BinDir`] arrives carrying the link's name — which is the
/// delegated executable's name, and the only thing telling that invocation apart from
/// the one libkrun makes.
///
/// The comparison is against [`contract::GUEST_BIN_PATH`] rather than the crate's binary
/// name, because the name libkrun runs this under is the path the host wrote it to and
/// not the name cargo built it as.
fn role() -> Role {
    let argv0 = std::env::args().next().unwrap_or_default();
    let ours = Path::new(contract::GUEST_BIN_PATH).file_name();
    match Path::new(&argv0).file_name() {
        Some(name) if Some(name) != ours => Role::Shim(name.to_string_lossy().into_owned()),
        // No `argv[0]` at all: nothing claims we are a shim, so serve.
        _ => Role::Agent,
    }
}

/// Both roles wait on something they do not drive — a port, a socket, a command — so both
/// are async. The runtime is built by hand rather than by `#[tokio::main]` because the
/// init work below it is not: `pivot_root` moves the root out from under every thread in
/// the process, and it is easier to reason about when there is only one.
fn main() -> ExitCode {
    match role() {
        // A shim exits with somebody else's code — the delegated executable's — which is
        // the whole point of it.
        Role::Shim(name) => runtime().block_on(shim::run(&name)),

        Role::Agent => {
            let port = match init::prepare() {
                Ok(port) => port,
                Err(e) => {
                    eprintln!("cortex-uvm-guest: {e}");
                    return ExitCode::from(NOT_EXECUTABLE);
                }
            };
            // A session's own answers travel inside its responses, so this code says
            // only whether the session itself worked.
            match runtime().block_on(agent::run(port)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("cortex-uvm-guest: {e}");
                    ExitCode::from(NOT_EXECUTABLE)
                }
            }
        }
    }
}

/// Multi-threaded, because the agent's work is: an execution waits on its command and on
/// the shims that command dials, and a delegated call is answered while the command that
/// made it is still running. A shim is one round trip and would not have noticed either
/// way.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime")
}
