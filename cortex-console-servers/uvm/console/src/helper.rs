//! The executable a boot actually runs, which is never this one.
//!
//! A boot is `cortex-uvm-boot`, embedded in this binary by the build script and written out
//! to a cache the first time a session needs it. Two reasons it is a file rather than a
//! process this one becomes:
//!
//! - **It has to be signed.** Creating a VM through Hypervisor.framework needs the
//!   `com.apple.security.hypervisor` entitlement, and libkrun `dlopen`s libkrunfw, which needs
//!   library validation off. Both are carried by a code signature — and `cargo build` produces
//!   an unsigned binary, and relinking drops whatever was there before. Something has to be
//!   written and signed on the way to a boot no matter how this is arranged.
//! - **It is not this binary.** Which is what makes the signed artifact small, and keeps the
//!   VMM and the network stack behind it out of the process that only answers a session.
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

use std::path::PathBuf;
use std::sync::OnceLock;

/// The boot half, built for this host and embedded by `build.rs`.
const BOOT_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cortex-uvm-boot"));

/// The entitlements the boot process needs, and only those.
#[cfg(target_os = "macos")]
const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <!-- Required to create a VM via Hypervisor.framework (libkrun/HVF). -->
    <key>com.apple.security.hypervisor</key>
    <true/>
    <!-- libkrun dlopen()s libkrunfw at runtime; allow unsigned/other-team libs. -->
    <key>com.apple.security.cs.disable-library-validation</key>
    <true/>
</dict>
</plist>
"#;

/// The program a boot runs, written out and — on macOS — signed, once per build.
pub fn boot_helper() -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let cache = crate::assets::home()?.join("boot-helper");
    std::fs::create_dir_all(&cache)?;

    let dest = cache.join(format!("boot-{}", key()));
    if dest.exists() {
        return Ok(dest);
    }

    // Written, made executable and signed at a path nothing else will pick, then renamed
    // into place. Two boots racing here both do the work and the rename decides which copy
    // survives — where sharing one temporary path would mean one of them signing a file the
    // other was still writing.
    let tmp = cache.join(format!(
        "boot-{}.{}.{}.tmp",
        key(),
        std::process::id(),
        seq()
    ));
    std::fs::write(&tmp, BOOT_BIN)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;

    #[cfg(target_os = "macos")]
    if let Err(e) = sign(&cache, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

#[cfg(target_os = "macos")]
fn sign(cache: &std::path::Path, binary: &std::path::Path) -> anyhow::Result<()> {
    let plist = cache.join("entitlements.plist");
    std::fs::write(&plist, ENTITLEMENTS)?;

    let status = std::process::Command::new("codesign")
        .args(["-s", "-", "--force", "--entitlements"])
        .arg(&plist)
        .arg(binary)
        .status()?;

    anyhow::ensure!(
        status.success(),
        "codesign of the boot helper failed ({status}) — without the hypervisor entitlement a \
         boot cannot create a VM"
    );
    Ok(())
}

/// A cache key that changes when the embedded binary does: its content hash and its length.
///
/// Computed once per process. The bytes are a compile-time constant, so every call after the
/// first would hash the same input to the same answer.
fn key() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| {
        use sha2::{Digest as _, Sha256};
        let hash = format!("{:x}", Sha256::digest(BOOT_BIN));
        format!("{}-{}", &hash[..16], BOOT_BIN.len())
    })
}

/// Distinguishes two boots of one process, which share a pid and a key.
fn seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}
