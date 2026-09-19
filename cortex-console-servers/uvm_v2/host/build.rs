//! Cross-compile the guest half and leave it where the host can embed it.
//!
//! The two are built apart because they run on different platforms: this half is a Mach-O
//! (or an ELF) for the machine you are sitting at, and the other is a static `musl` ELF for
//! the linux inside the VM. One `cargo build` produces one target, so the guest needs a cargo
//! invocation of its own — and a target directory of its own, or the two would wait on each
//! other's lock.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() -> anyhow::Result<()> {
    println!("cargo::rerun-if-changed=build.rs");

    let out = PathBuf::from(std::env::var("OUT_DIR")?).join("cortex-uvm-v2-guest");

    // A binary already built, for anyone who would rather not wait for a nested build on
    // every change to this half.
    println!("cargo::rerun-if-env-changed=CORTEX_UVM_GUEST_BIN");
    if let Some(prebuilt) = std::env::var_os("CORTEX_UVM_GUEST_BIN") {
        let prebuilt = PathBuf::from(prebuilt);
        println!("cargo::rerun-if-changed={}", prebuilt.display());
        return std::fs::copy(&prebuilt, &out)
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("CORTEX_UVM_GUEST_BIN={}: {e}", prebuilt.display()));
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
    // The guest links cortex, so a change to the protocol is a change to the guest.
    println!("cargo::rerun-if-changed=../../../../cortex/src");

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
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(&cargo)
        // From the crate's own directory, so its `.cargo/config.toml` — which names the
        // linker that can produce an ELF here — is the configuration in effect.
        .current_dir(&crate_dir)
        .args(["build", "--release", "--target", target])
        .env("CARGO_TARGET_DIR", &target_dir)
        // This build's choices are for this build's target.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .status()
        .map_err(|e| anyhow::anyhow!("running cargo in {}: {e}", crate_dir.display()))?;
    anyhow::ensure!(
        status.success(),
        "building cortex-uvm-v2-guest for {target} failed.\n\
         If the target is not installed: rustup target add {target}"
    );

    let built: &Path = &target_dir.join(format!("{target}/release/cortex-uvm-v2-guest"));
    std::fs::copy(built, &out)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("copying {} to {}: {e}", built.display(), out.display()))
}
