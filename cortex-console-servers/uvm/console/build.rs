//! Cross-compile the guest half and leave it in `OUT_DIR`, so the console binary can
//! `include_bytes!` it.
//!
//! `cortex-uvm-guest` runs inside the micro-VM, which is always Linux and always the
//! host's architecture, so it is built for `<arch>-unknown-linux-musl` — statically
//! linked, because the base image is whatever the caller chose and may share no libc with
//! anything. Embedding the result rather than shipping it alongside is what keeps
//! `cortex-uvm-console` a single file: a boot writes the bytes into the boot root it just
//! made, and there is nothing to install, find or version-match at run time.
//!
//! # The nested cargo, and why it is not a workspace member
//!
//! The guest crate is its own workspace root, so a bare `cargo build` here never tries to
//! compile `pivot_root` for darwin. That means it is built by invoking cargo again, from
//! its own directory — which is also what lets its `.cargo/config.toml` (the ELF linker
//! for the musl targets) be found at all.
//!
//! The nested build gets a target directory of its own under `OUT_DIR` and none of this
//! build's flags. A shared target directory would deadlock on cargo's own lock, and
//! inherited `RUSTFLAGS` or a `RUSTC_WRAPPER` would be applied to a target they were not
//! chosen for.
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
//! guest that will not start. `CORTEX_UVM_GUEST_BIN` short-circuits the whole thing and
//! embeds the file it names, for a caller who builds the guest some other way.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn main() -> anyhow::Result<()> {
    let out = PathBuf::from(std::env::var("OUT_DIR")?).join("cortex-uvm-guest");

    println!("cargo::rerun-if-env-changed=CORTEX_UVM_GUEST_BIN");
    if let Some(prebuilt) = std::env::var_os("CORTEX_UVM_GUEST_BIN") {
        let prebuilt = PathBuf::from(prebuilt);
        println!("cargo::rerun-if-changed={}", prebuilt.display());
        std::fs::copy(&prebuilt, &out)
            .map_err(|e| anyhow::anyhow!("CORTEX_UVM_GUEST_BIN={}: {e}", prebuilt.display()))?;
        return Ok(());
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
    println!("cargo::rerun-if-changed=../../../cortex/src");

    let built = build(&crate_dir)?;
    std::fs::copy(&built, &out)
        .map_err(|e| anyhow::anyhow!("copying {} to {}: {e}", built.display(), out.display()))?;
    Ok(())
}

/// Build the guest crate for the musl target that matches this host, and say where the
/// binary landed.
fn build(crate_dir: &Path) -> anyhow::Result<PathBuf> {
    // A libkrun guest runs the host's architecture: there is no emulation in the middle
    // to make anything else work.
    let target = match std::env::consts::ARCH {
        "aarch64" => "aarch64-unknown-linux-musl",
        "x86_64" => "x86_64-unknown-linux-musl",
        other => anyhow::bail!("no guest target for a {other} host"),
    };
    let target_dir = PathBuf::from(std::env::var("OUT_DIR")?).join("guest");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        // From the guest crate, so its `.cargo/config.toml` is the one in effect.
        .current_dir(crate_dir)
        .args(["build", "--release", "--target", target])
        .env("CARGO_TARGET_DIR", &target_dir)
        // This build's choices are for this build's target.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .status()
        .map_err(|e| anyhow::anyhow!("running cargo for the guest: {e}"))?;

    anyhow::ensure!(
        status.success(),
        "building cortex-uvm-guest for {target} failed.\n\
         If the target is not installed: rustup target add {target}"
    );

    Ok(target_dir.join(target).join("release/cortex-uvm-guest"))
}
