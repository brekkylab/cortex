//! A micro-VM, booted on an image built here.
//!
//! Two things start one, and they differ in how long it lives rather than in what it does.
//! A session boots once and runs whatever its client asks for until the client is done; a
//! build's `RUN` wants one command and the filesystem it left behind. So the long-lived one
//! is a value, and the short one is a function over it.
//!
//! Either way the guest is `cortex-uvm-v2-guest`, which libkrun execs as the first userspace
//! process — in `boot` mode for a session, `exec` mode for a build step.

use std::path::PathBuf;

use crate::rootfs::Digest;

/// What a command exited with, and what it wrote.
pub struct Exit {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A host directory the guest can see, and where it sees it.
pub struct Mount {
    pub at: String,
    pub from: PathBuf,
}

/// A booted micro-VM, held for as long as there is something to ask it.
///
/// Dropping it takes the machine down and removes the scratch it was given, which is what
/// makes a session's writes go no further than the session.
pub struct Uvm {
    _private: (),
}

impl Uvm {
    /// Boot `image` and wait for the guest to say it is there.
    ///
    /// Returns once the guest has built its root and opened the port, not merely once the
    /// machine is running — there is nothing a caller can do with the time in between.
    pub async fn boot(image: &Digest, mounts: &[Mount]) -> anyhow::Result<Uvm> {
        todo!(
            "stitch {image} into a disk, format the upper, write the boot root, start the machine with {} mount(s)",
            mounts.len()
        )
    }

    /// Run one command, with what the image states already in front of it.
    pub async fn exec(&mut self, argv: &[String], timeout_ms: Option<u64>) -> anyhow::Result<Exit> {
        todo!("{argv:?} with {timeout_ms:?}")
    }

    /// Everything written since the boot, kept as a layer.
    pub async fn commit(&mut self) -> anyhow::Result<Digest> {
        todo!("the guest's upperdir, as an EROFS in the store")
    }
}

/// Boot `image`, run `argv` once, and hand back what it exited with and what it wrote.
///
/// What a build's `RUN` wants: the guest is started in `exec` mode, so there is no port and
/// no session — one command, one answer, one layer.
pub async fn run(image: &Digest, argv: &[String]) -> anyhow::Result<(Exit, Digest)> {
    todo!("{argv:?} on {image}, once")
}
