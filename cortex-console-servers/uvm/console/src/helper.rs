//! The executable a boot actually runs, which on macOS is not this one.
//!
//! Creating a VM through Hypervisor.framework needs the `com.apple.security.hypervisor`
//! entitlement, and libkrun `dlopen`s libkrunfw, which needs library validation off. Both
//! are carried by a code signature — and `cargo build` produces an unsigned binary, and
//! relinking drops whatever was there before, so a console server cannot simply be the
//! process that boots.
//!
//! So it is not. A boot runs a **copy of this binary**, signed once and cached, in the
//! [`Boot`](crate::Role::Boot) role. The copy is keyed by the content of the original, so
//! a rebuild produces a different key and therefore a fresh copy, and two consoles built
//! from the same binary share one.
//!
//! On anything but macOS there is nothing to sign and no reason to copy, so a boot runs
//! this binary where it already is.
//!
//! # Why signing a copy rather than asking the caller to sign
//!
//! Because the caller would have to sign *their* binary. A console server is started by
//! whoever wants a console — a test, an agent runtime, a CLI — and the entitlement is a
//! property of the process that calls `hv_vm_create`, not of the product. Putting the
//! requirement on this crate's own artifact keeps it out of everyone else's build.

#[cfg(target_os = "macos")]
use std::io;
use std::path::PathBuf;

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

/// The program a boot child runs.
#[cfg(not(target_os = "macos"))]
pub fn boot_helper() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_exe()?)
}

/// The program a boot child runs: a signed copy of this binary, made once per build.
#[cfg(target_os = "macos")]
pub fn boot_helper() -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let exe = std::env::current_exe()?;
    let cache = crate::assets::home()?.join("boot-helper");
    std::fs::create_dir_all(&cache)?;

    let dest = cache.join(format!("boot-{}", key(&exe)?));
    if dest.exists() {
        return Ok(dest);
    }

    // Written, made executable and signed at a path nothing else will pick, then renamed
    // into place. Two boots racing here both do the work and the rename decides which
    // copy survives — where sharing one temporary path would mean one of them signing a
    // file the other was still writing.
    let tmp = cache.join(format!("boot-{}.{}.tmp", key(&exe)?, std::process::id()));
    std::fs::copy(&exe, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;

    let plist = cache.join("entitlements.plist");
    std::fs::write(&plist, ENTITLEMENTS)?;
    let status = std::process::Command::new("codesign")
        .args(["-s", "-", "--force", "--entitlements"])
        .arg(&plist)
        .arg(&tmp)
        .status()?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!(
            "codesign of the boot helper failed ({status}) — without the hypervisor \
             entitlement a boot cannot create a VM"
        );
    }

    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// A cache key that changes when the binary does: its content hash and its length.
///
/// Computed once per process, because it reads the whole executable and every boot after
/// the first would get the same answer.
#[cfg(target_os = "macos")]
fn key(exe: &std::path::Path) -> io::Result<&'static str> {
    use std::sync::OnceLock;
    static KEY: OnceLock<String> = OnceLock::new();

    if let Some(key) = KEY.get() {
        return Ok(key);
    }

    use sha2::{Digest as _, Sha256};
    let mut file = std::fs::File::open(exe)?;
    let mut hasher = Sha256::new();
    let len = io::copy(&mut file, &mut hasher)?;
    let hash = format!("{:x}", hasher.finalize());

    Ok(KEY.get_or_init(|| format!("{}-{len}", &hash[..16])))
}
