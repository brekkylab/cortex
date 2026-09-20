//! The other two halves, which are files beside this binary rather than parts of it.
//!
//! `build.rs` builds both and leaves them in the directory cargo linked this binary into, so
//! the three ship as a set and a lookup is `current_exe`'s parent plus a name. Nothing is
//! searched for on `PATH`: a host that found *some* `cortex-uvm-v2-boot` would be a host
//! running a different build's hypervisor against this build's contract, and the contract is
//! a command line the two ends have to agree on exactly.
//!
//! # Why they are separate binaries at all
//!
//! - **The boot has to be signed.** Creating a VM through Hypervisor.framework needs the
//!   `com.apple.security.hypervisor` entitlement, and libkrun `dlopen`s libkrunfw, which needs
//!   library validation off. Both are carried by a code signature, and a signature is read at
//!   `exec` — so the process that creates a VM can never be the process that decided to.
//!   Signing a copy of *this* binary instead would mean signing an image store, a registry
//!   client and a console server to give a hypervisor to the one function that wants it.
//! - **The boot is not this binary.** Which is what keeps the VMM and the network stack out
//!   of the process that only answers a session: with the boot a package of its own, they are
//!   not in this one's dependency graph at all, rather than merely unreached by it.
//! - **The guest is not this platform.** It is a static musl ELF that runs as the first
//!   userspace process inside the VM. There is no arrangement in which those bytes are code
//!   this binary could call.
//!
//! # The overrides
//!
//! Each half can be named outright, which is what a caller who builds one some other way — or
//! ships the three somewhere with a layout of its own — has instead of this rule. The same
//! two variables are read by `build.rs`, where they name what to place rather than what to
//! run.

use std::path::PathBuf;

/// The program a boot runs: `$CORTEX_UVM_V2_BOOT_BIN`, else the signed binary beside this one.
pub fn boot() -> anyhow::Result<PathBuf> {
    beside("CORTEX_UVM_V2_BOOT_BIN", "cortex-uvm-v2-boot")
}

/// The program libkrun execs inside the VM: `$CORTEX_UVM_V2_GUEST_BIN`, else the binary
/// beside this one. Copied into every boot root — see [`uvm`](super::uvm).
pub fn guest() -> anyhow::Result<PathBuf> {
    beside("CORTEX_UVM_V2_GUEST_BIN", "cortex-uvm-v2-guest")
}

/// One half, by the variable naming it or by its name next to this binary.
///
/// Both cases are checked rather than returned on trust: this path is about to be spawned or
/// copied into a guest's root, and a missing file reported here says which half and where it
/// was looked for, where the same file missing a moment later is a boot that failed to start.
fn beside(env: &str, name: &str) -> anyhow::Result<PathBuf> {
    if let Some(named) = std::env::var_os(env) {
        let path = PathBuf::from(named);
        anyhow::ensure!(path.is_file(), "{env}={} is not a file", path.display());
        return Ok(path);
    }

    // Resolved, not as invoked: a symlink on `PATH` says nothing about where the rest of the
    // set is, and the real file is what `build.rs` put them next to.
    let exe =
        std::env::current_exe().map_err(|e| anyhow::anyhow!("asking where this binary is: {e}"))?;
    let path = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no directory to look in", exe.display()))?
        .join(name);
    anyhow::ensure!(
        path.is_file(),
        "no {name} beside {} — build it with this binary (`cargo build -p \
         cortex-uvm-v2-host`), or name one in {env}",
        exe.display()
    );
    Ok(path)
}
