//! Build the two halves this one cannot build itself, and leave them beside it.
//!
//! | | target | what it is |
//! |---|---|---|
//! | `cortex-uvm-v2-guest` | `<arch>-unknown-linux-musl` | it runs *inside* the VM |
//! | `cortex-uvm-v2-boot` | this host, signed here on macOS | it is what gets run *as* the VM |
//!
//! Both land next to the binary cargo is building — `target/<profile>/`, the directory
//! `OUT_DIR` sits three levels under — so that a `cargo build` of the host produces all three
//! files at once and the host can find either of them by looking next to itself. See
//! [`session::halves`](../src/session/halves.rs) for the other end of that.
//!
//! # Beside rather than inside
//!
//! Both could be `include_bytes!`d instead, which would make the host one file. What that
//! costs is a copy of each on the way to every boot, a host binary that carries two
//! executables it never runs itself, and — for the boot half — a signature that cannot be
//! applied until run time, because bytes in a `.rodata` section are not a file anything can
//! `codesign`. Three files that ship together is the cheaper arrangement: the boot half is
//! signed once, here, and a boot root gets the guest by copying a file rather than by
//! writing a few megabytes out of this binary's own image.
//!
//! # A cargo of its own for each, and why
//!
//! Neither can be built by the cargo that is building this crate.
//!
//! The guest is a different platform — a static `musl` ELF for the linux inside the VM, where
//! this half is whatever the machine you are sitting at runs. One `cargo build` produces one
//! target. It is also its own workspace root, so that a bare `cargo build` never tries to
//! compile `pivot_root` for darwin, and being invoked from its own directory is what lets its
//! `.cargo/config.toml` name a linker that can produce an ELF here.
//!
//! The boot half builds for *this* host and is a workspace member, but its binary sits behind
//! a `vm` feature that pulls in a VMM. Enabling that feature from here would put a hypervisor
//! in this crate's dependency graph, which is the thing splitting the binary was for.
//!
//! Each nested build gets a target directory of its own under `OUT_DIR` and none of this
//! build's flags. A shared target directory would deadlock on cargo's own lock, and inherited
//! `RUSTFLAGS` or a `RUSTC_WRAPPER` would be applied to a target they were not chosen for.
//!
//! # What this needs installed
//!
//! The musl target, and nothing else:
//!
//! ```sh
//! rustup target add aarch64-unknown-linux-musl   # or x86_64-…, matching the host
//! ```
//!
//! A build without it fails here with that line in the message, rather than later with a
//! guest that will not start. `CORTEX_UVM_V2_GUEST_BIN` and `CORTEX_UVM_V2_BOOT_BIN` name a
//! file to place instead of building one, for a caller who builds either half some other way.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn main() -> anyhow::Result<()> {
    println!("cargo::rerun-if-changed=build.rs");
    let beside = beside()?;
    guest(&beside)?;
    boot(&beside)
}

/// Where cargo is putting this crate's binaries: `OUT_DIR` is
/// `<target>/<profile>/build/<pkg>-<hash>/out`, so the profile directory is three up.
///
/// Derived rather than asked for, because cargo tells a build script where its own output
/// goes and nothing else — there is no variable naming the directory the linked binary lands
/// in, and reconstructing it from `CARGO_TARGET_DIR`, the profile name and a `--target` that
/// may or may not have been passed is the same answer with more ways to be wrong.
fn beside() -> anyhow::Result<PathBuf> {
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    out.ancestors()
        .nth(3)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            anyhow::anyhow!("OUT_DIR {} is not inside a target directory", out.display())
        })
}

/// Cross-compile the guest half and leave it beside this binary.
fn guest(beside: &Path) -> anyhow::Result<()> {
    let out = beside.join("cortex-uvm-v2-guest");

    // A binary already built, for anyone who would rather not wait for a nested build on
    // every change to this half.
    println!("cargo::rerun-if-env-changed=CORTEX_UVM_V2_GUEST_BIN");
    if let Some(prebuilt) = std::env::var_os("CORTEX_UVM_V2_GUEST_BIN") {
        let prebuilt = PathBuf::from(prebuilt);
        println!("cargo::rerun-if-changed={}", prebuilt.display());
        return place(&prebuilt, &out, false);
    }

    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?)
        .join("../guest")
        .canonicalize()?;
    for source in ["src", "Cargo.toml", ".cargo/config.toml"] {
        println!(
            "cargo::rerun-if-changed={}",
            crate_dir.join(source).display()
        );
    }
    // The guest links cortex and the contract, so a change to either is a change to the
    // guest. Cargo would rebuild them inside the nested build; what this buys is that the
    // nested build is run at all.
    println!("cargo::rerun-if-changed=../../../cortex/src");
    println!("cargo::rerun-if-changed=../common/src");

    // A libkrun guest runs the host's architecture: there is no emulation in the middle to
    // make anything else work.
    let target = match std::env::consts::ARCH {
        "aarch64" => "aarch64-unknown-linux-musl",
        "x86_64" => "x86_64-unknown-linux-musl",
        other => anyhow::bail!("no guest target for a {other} host"),
    };
    let target_dir = PathBuf::from(std::env::var("OUT_DIR")?).join("guest");

    // `--release` because this file is copied into a boot root on the way to every boot, so
    // its size is paid more than once.
    let status = cargo(&crate_dir, &target_dir)
        .args(["build", "--release", "--target", target])
        .status()
        .map_err(|e| anyhow::anyhow!("running cargo in {}: {e}", crate_dir.display()))?;
    anyhow::ensure!(
        status.success(),
        "building cortex-uvm-v2-guest for {target} failed.\n\
         If the target is not installed: rustup target add {target}"
    );

    let built: &Path = &target_dir.join(format!("{target}/release/cortex-uvm-v2-guest"));
    place(built, &out, false)
}

/// Build the boot half for *this* host, sign it, and leave it beside this binary.
///
/// `--release` because it is spawned on the way to every boot and a debug VMM is slower to
/// load for no gain in a half nothing steps through.
fn boot(beside: &Path) -> anyhow::Result<()> {
    let out = beside.join("cortex-uvm-v2-boot");

    println!("cargo::rerun-if-env-changed=CORTEX_UVM_V2_BOOT_BIN");
    if let Some(prebuilt) = std::env::var_os("CORTEX_UVM_V2_BOOT_BIN") {
        let prebuilt = PathBuf::from(prebuilt);
        println!("cargo::rerun-if-changed={}", prebuilt.display());
        // Signed like a built one: what a boot needs is an entitlement, and where the file
        // came from does not change that.
        return place(&prebuilt, &out, true);
    }

    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?)
        .join("../boot")
        .canonicalize()?;
    for source in ["src", "Cargo.toml"] {
        println!(
            "cargo::rerun-if-changed={}",
            crate_dir.join(source).display()
        );
    }
    // The contract the boot parses its command line with — the same reason as the guest's.
    println!("cargo::rerun-if-changed=../common/src");

    let target_dir = PathBuf::from(std::env::var("OUT_DIR")?).join("boot");
    let status = cargo(&crate_dir, &target_dir)
        .args(["build", "--release", "--features", "vm"])
        .status()
        .map_err(|e| anyhow::anyhow!("running cargo in {}: {e}", crate_dir.display()))?;
    anyhow::ensure!(status.success(), "building cortex-uvm-v2-boot failed");

    place(&target_dir.join("release/cortex-uvm-v2-boot"), &out, true)
}

/// Put `built` at `out`, signing it on the way if it is the half that needs it.
///
/// Through a temporary name in the same directory and a rename, for two reasons. A rename is
/// atomic, so a host looking beside itself never finds a half-written file or one that is not
/// signed yet; and it replaces the *name* rather than the bytes, so a boot spawned from the
/// previous build goes on running its own inode instead of having a copy land on top of an
/// executable that is currently mapped.
fn place(built: &Path, out: &Path, sign_it: bool) -> anyhow::Result<()> {
    let part = out.with_extension(format!("{}.part", std::process::id()));
    std::fs::copy(built, &part)
        .map_err(|e| anyhow::anyhow!("copying {} to {}: {e}", built.display(), part.display()))?;

    if sign_it && let Err(e) = sign(&part) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }

    std::fs::rename(&part, out)
        .map_err(|e| anyhow::anyhow!("renaming {} to {}: {e}", part.display(), out.display()))
}

// Creating a VM through Hypervisor.framework needs `com.apple.security.hypervisor`, and
// libkrun `dlopen`s libkrunfw, which needs library validation off. Both are carried by a code
// signature, and `cargo build` produces an unsigned binary — so the boot half is signed here,
// after the nested build that linked it and before anything can spawn it. Entitlements are
// read at `exec`, so this has to happen to the file and cannot be done to a running process.
#[cfg(target_os = "macos")]
const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.hypervisor</key>
    <true/>
    <key>com.apple.security.cs.disable-library-validation</key>
    <true/>
</dict>
</plist>
"#;

/// Sign `binary` ad-hoc with the entitlements a boot needs.
///
/// Ad-hoc — `-s -` — because the entitlements are what matters and who signed them is not
/// checked on the machine that runs them. Which is also why signing belongs to whoever builds
/// this and not to whoever runs it: a console server is started by a test, an agent runtime
/// or a CLI, and an entitlement is a property of the process that calls `hv_vm_create` rather
/// than of the product around it.
#[cfg(target_os = "macos")]
fn sign(binary: &Path) -> anyhow::Result<()> {
    let plist = binary.with_extension("entitlements.plist");
    std::fs::write(&plist, ENTITLEMENTS)?;

    let signed = Command::new("codesign")
        .args(["-s", "-", "--force", "--entitlements"])
        .arg(&plist)
        .arg(binary)
        .status();
    let _ = std::fs::remove_file(&plist);

    let signed = signed?;
    anyhow::ensure!(
        signed.success(),
        "codesign of {} failed ({signed}) — without the hypervisor entitlement a boot cannot \
         create a VM",
        binary.display()
    );
    Ok(())
}

/// Nothing to sign. Entitlements are how macOS grants access to Hypervisor.framework; every
/// other host either lets a process open `/dev/kvm` or does not, and no property of the file
/// changes that.
#[cfg(not(target_os = "macos"))]
fn sign(_binary: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// A cargo to run in `crate_dir`, with a target directory of its own and none of this build's
/// choices — those were made for this build's target.
fn cargo(crate_dir: &Path, target_dir: &Path) -> Command {
    let mut cargo = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    // From the crate's own directory, so its `.cargo/config.toml` — where it has one — is the
    // configuration in effect.
    cargo
        .current_dir(crate_dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    cargo
}
