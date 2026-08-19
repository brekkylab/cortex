//! `cortex-uvm-console` — a console server whose "somewhere" is a micro-VM.
//!
//! Answers a console session on stdin and stdout: `init` to say which names the session
//! delegates, an `exec` per command, `quit` to end. See [`cortex::console`] for the shape
//! of what those descriptors carry. What differs from a host-local console is only where a
//! command runs — inside a Linux guest, on a filesystem of its own, on a kernel this
//! process brought up — and everything below follows from that one difference.
//!
//! # Booting is a real cost here, which is what `start` and `stop` are for
//!
//! On a host-local backend, booting is a directory of symlinks and the pair is a nicety. On
//! this one it is a kernel, an overlay of two block devices and a device probe: seconds,
//! and a few hundred megabytes of the host's memory for as long as the guest is up.
//!
//! Neither is required and neither unlocks anything — the first `exec`, `read` or `write`
//! to find a session without a guest is served by a session that has just booted one. What
//! they buy is *who waits* and *what is occupied while nothing runs*, and on this backend
//! both are worth a message.
//!
//! # Two roles, one binary
//!
//! `msb_krun`'s `enter` never returns: when the guest shuts down the VMM calls `_exit` and
//! the process is gone. A console server has a session to keep answering, so it cannot be
//! the process that boots — and this binary is therefore reached two ways, told apart by
//! [`Role`]:
//!
//! - with no arguments — **server**: answer the session, and boot children as it needs them
//!   (see [`server`]).
//! - with `--boot` — **boot child**: assemble the VM described by the environment and enter
//!   it (see [`boot`]).
//!
//! On macOS the child is not this file but a signed copy of it, because creating a VM needs
//! an entitlement that `cargo build` does not produce — see [`helper`].
//!
//! # What the guest is
//!
//! ```text
//! /                a read-only base image, overlaid with this session's ext4 upper
//! /the/host/path   the session's tree, shared over virtio-fs from the host directory
//!                  of that same name
//! ```
//!
//! Writes anywhere in the guest land on the session's upper and stay there for as long as
//! the guest is up; they are gone when it is released, because the image is deleted with
//! it. The base image is shared, never written, and provisioned once — see [`assets`].
//!
//! What that base *is* comes from the environment: an OCI reference
//! ([`IMAGE_ENV`](assets::IMAGE_ENV)) is pulled from a registry, and a caller who names none
//! gets the pinned rootfs the crate falls back to. Either way it reaches the guest as one
//! read-only disk, and what the image says about running a process in it reaches the commands
//! — see [`ImageSpec`](contract::ImageSpec).
//!
//! The tree is optional and comes from the client, not the environment: an `init` names it
//! as a `file://` URL, and what that names is a directory on this host — mounted there by
//! whoever built it, which is not this crate's business. The boot shares it (see
//! [`WORKFS_ENV`](contract::WORKFS_ENV)) **at the path the host spells it with**, so a
//! command's working directory inside the guest is a path the client can open. That is
//! what makes this backend answer the same protocol as the host-local one rather than a
//! translated dialect of it.
//!
//! # What this backend deliberately does not do
//!
//! **No networking.** No virtio-net device is attached, so the guest reaches nothing.
//! Egress needs a device and a policy to go with it, and a policy needs a way for a caller
//! to say what it wants — which this server has no channel for, since a console session
//! says nothing about a network.
//!
//! **No snapshots.** A session's filesystem is thrown away when its guest is released.
//! Persisting it is a real feature and not a hard one — the upper is a sparse file, so a
//! snapshot is its allocated extents — but nothing in the protocol asks for one, so there
//! is nowhere for a client to say when.
//!
//! **No timeout.** An `exec` carries a `timeout_ms` and it is read and not applied, the
//! same gap [`cortex-local-console`] has. A command that never ends is one the guest agent
//! waits on forever, and this relay waits with it.
//!
//! [`cortex-local-console`]: https://docs.rs/cortex-local-console

mod assets;
mod boot;
mod contract;
mod helper;
mod server;

use std::process::ExitCode;

/// A failure of ours, not the command's — the shell's code for "found it, could not run
/// it".
const NOT_EXECUTABLE: u8 = 126;

/// Which of the two jobs this process was started to do.
enum Role {
    /// Answer the session.
    Server,
    /// Become a micro-VM.
    Boot,
}

/// Decide the role from the arguments.
///
/// The boot child is a copy of this binary at a cache path, so `argv[0]` says nothing
/// useful about which role it is — unlike a shim, whose whole identity is the name it was
/// invoked under. An explicit argument is what is left, and it is one nobody types: a boot
/// is started by [`server`] and by nothing else.
fn role() -> Role {
    match std::env::args().nth(1) {
        Some(arg) if arg == server::BOOT_ARG => Role::Boot,
        _ => Role::Server,
    }
}

/// A server waits on a pipe, a socket and a guest, so it is async. A boot child is not: it
/// assembles a VM and hands the process to the VMM, and everything it waits for after that
/// it waits for by not existing.
fn main() -> ExitCode {
    match role() {
        Role::Boot => match boot::run() {
            // `enter` only returns on success by not returning at all: the `Ok` holds an
            // `Infallible`, and the empty match is what says so.
            Ok(never) => match never {},
            Err(e) => {
                eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
                ExitCode::from(NOT_EXECUTABLE)
            }
        },

        // Each command's code travels back inside its own answer, so this one only says
        // whether the session itself worked.
        Role::Server => match runtime().block_on(server::run()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
                ExitCode::from(NOT_EXECUTABLE)
            }
        },
    }
}

/// Multi-threaded, because a relay's waiting overlaps: the channel to the client, the
/// channel into the guest, a boot child being reaped, and the blocking work of formatting
/// an image or hashing a download.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime")
}
