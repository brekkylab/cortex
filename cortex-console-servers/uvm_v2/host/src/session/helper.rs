//! The executable a boot actually runs, which is never this one.
//!
//! A boot is `cortex-uvm-v2-boot`, built for this host by the build script, embedded in this
//! binary and written out to a cache the first time a session needs one. Two reasons it is a
//! separate file rather than a mode this process re-execs itself in:
//!
//! - **It has to be signed.** Creating a VM through Hypervisor.framework needs the
//!   `com.apple.security.hypervisor` entitlement, and libkrun `dlopen`s libkrunfw, which needs
//!   library validation off. Both are carried by a code signature — `cargo build` produces an
//!   unsigned binary, and relinking drops whatever was there before — and a signature is read
//!   at `exec`. So something is written and signed on the way to a boot however this is
//!   arranged, and signing a copy of *this* binary would mean signing an image store, a
//!   registry client and a console server to give a hypervisor to the one function that wants
//!   it.
//! - **It is not this binary.** Which is what keeps the VMM and the network stack out of the
//!   process that only answers a session: with the boot a package of its own, they are not in
//!   this one's dependency graph at all, rather than merely unreached by it.
//!
//! The copy is keyed by the content of the embedded bytes, so a rebuild produces a different
//! key and therefore a fresh copy, and two servers built from the same source share one.
//!
//! # Why signing here rather than asking the caller to sign
//!
//! Because the caller would have to sign *their* binary. A console server is started by
//! whoever wants a console — a test, an agent runtime, a CLI — and the entitlement is a
//! property of the process that calls `hv_vm_create`, not of the product. Keeping the
//! requirement on an artifact this crate carries keeps it out of everyone else's build.

use std::{path::PathBuf, sync::OnceLock};

use crate::rootfs::{Digest, home};

/// The boot half, built for this host and embedded by `build.rs`.
const BOOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cortex-uvm-v2-boot"));

/// The program a boot runs, written out and — on macOS — signed, once per build.
pub fn boot_binary() -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    let cache = home().join("boot");
    std::fs::create_dir_all(&cache)?;

    let machine = cache.join(key());
    if machine.is_file() {
        return Ok(machine);
    }

    // Written, made executable and signed at a path nothing else will pick, then renamed
    // into place — two boots racing here both do the work and the rename decides which copy
    // survives, where sharing one path would mean signing a file the other was still writing.
    let part = cache.join(format!("{}.{}.part", key(), std::process::id()));
    std::fs::write(&part, BOOT)?;
    std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))?;

    #[cfg(target_os = "macos")]
    if let Err(e) = super::entitlement::sign(&part) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }

    std::fs::rename(&part, &machine)?;
    Ok(machine)
}

/// A name that changes when the embedded binary does, so a rebuild is signed again and two
/// boots of one build share the copy.
///
/// Computed once per process: the bytes are a compile-time constant, so every call after the
/// first would hash the same input to the same answer.
fn key() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| Digest::of(BOOT).hex()[..16].to_string())
}
