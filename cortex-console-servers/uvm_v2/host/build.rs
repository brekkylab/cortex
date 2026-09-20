//! Build the two halves this binary carries inside it, and leave them where it can
//! `include_bytes!` them.
//!
//! | | target | why it is embedded |
//! |---|---|---|
//! | `cortex-uvm-v2-guest` | `<arch>-unknown-linux-musl` | it runs *inside* the VM |
//! | `cortex-uvm-v2-boot` | this host | it is what gets signed and run *as* the VM |
//!
//! Embedding rather than shipping either alongside is what keeps the host a single file: it
//! writes the guest into the boot root it just made, and the boot binary into a cache where it
//! signs it, so there is nothing to install, find or version-match at run time.
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

use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn main() -> anyhow::Result<()> {
    println!("cargo::rerun-if-changed=build.rs");
    guest()?;
    boot()
}

/// Cross-compile the guest half and leave it in `OUT_DIR`.
fn guest() -> anyhow::Result<()> {
    let out = PathBuf::from(std::env::var("OUT_DIR")?).join("cortex-uvm-v2-guest");

    // A binary already built, for anyone who would rather not wait for a nested build on
    // every change to this half.
    println!("cargo::rerun-if-env-changed=CORTEX_UVM_GUEST_BIN");
    if let Some(prebuilt) = std::env::var_os("CORTEX_UVM_GUEST_BIN") {
        let prebuilt = PathBuf::from(prebuilt);
        println!("cargo::rerun-if-changed={}", prebuilt.display());
        return place(&prebuilt, &out);
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

    // `--release` because these bytes are embedded and then written into a boot root on the
    // way to every boot, so their size is paid more than once.
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
    place(built, &out)
}

/// Build the boot half for *this* host and leave it in `OUT_DIR`.
///
/// `--release` because these bytes are embedded and then written out and signed on the way to
/// a boot, so their size is paid more than once.
fn boot() -> anyhow::Result<()> {
    let out = PathBuf::from(std::env::var("OUT_DIR")?).join("cortex-uvm-v2-boot");

    println!("cargo::rerun-if-env-changed=CORTEX_UVM_V2_BOOT_BIN");
    if let Some(prebuilt) = std::env::var_os("CORTEX_UVM_V2_BOOT_BIN") {
        let prebuilt = PathBuf::from(prebuilt);
        println!("cargo::rerun-if-changed={}", prebuilt.display());
        return place(&prebuilt, &out);
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

    place(&target_dir.join("release/cortex-uvm-v2-boot"), &out)
}

fn place(built: &Path, out: &Path) -> anyhow::Result<()> {
    std::fs::copy(built, out)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("copying {} to {}: {e}", built.display(), out.display()))
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
