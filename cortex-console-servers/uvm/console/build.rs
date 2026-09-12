//! Build the two halves this binary carries inside it, and leave them in `OUT_DIR` so it can
//! `include_bytes!` them.
//!
//! | | target | why it is embedded |
//! |---|---|---|
//! | `cortex-uvm-guest` | `<arch>-unknown-linux-musl` | it runs *inside* the VM |
//! | `cortex-uvm-boot` | this host | it is what gets signed and run *as* the VM |
//!
//! Embedding rather than shipping either alongside is what keeps `cortex-uvm-console` a single
//! file: a boot writes the guest into the boot root it just made, and a server writes the boot
//! binary into a cache and signs it, so there is nothing to install, find or version-match at
//! run time.
//!
//! The guest is statically linked because the base image is whatever the caller chose and may
//! share no libc with anything.
//!
//! # A cargo of its own for each, and why
//!
//! Neither can be built by the cargo that is building this crate.
//!
//! The guest is its own workspace root — so a bare `cargo build` never tries to compile
//! `pivot_root` for darwin — and being invoked from its own directory is also what lets its
//! `.cargo/config.toml` (the ELF linker for the musl targets) be found at all.
//!
//! The boot half *is* a workspace member, but its binary sits behind a `vm` feature that pulls
//! in a VMM. Enabling that feature from here would put a hypervisor in this crate's dependency
//! graph, which is the thing splitting the binary was for.
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
//! guest that will not start. `CORTEX_UVM_GUEST_BIN` and `CORTEX_UVM_BOOT_BIN` short-circuit
//! either half and embed the file they name, for a caller who builds one some other way.
//!
//! # The FUSE-T rpath
//!
//! One more thing this leaves in the binary, and nothing to do with the two halves above:
//! an `LC_RPATH` for `libfuse-t.dylib`. See [`fuse_t_rpath`].

use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn main() -> anyhow::Result<()> {
    println!("cargo::rerun-if-changed=build.rs");
    fuse_t_rpath();
    guest()?;
    boot()?;
    Ok(())
}

/// Give this binary an `LC_RPATH` for FUSE-T, which nothing else can.
///
/// `libfuse-t.dylib`'s install name is `@rpath/libfuse-t.dylib`, so anything linking it needs
/// an rpath saying where that is. This binary links it without ever asking to: `cortex/fuse-t`
/// is a feature of a *dependency*, and a workspace build that enables it anywhere — `cargo
/// build --all-features`, a sibling member, a test — unifies it into the cortex this links.
/// Three things then conspire to leave the rpath to this file:
///
/// - `fuse-t.pc` asks for it (`-Wl,-rpath,/usr/local/lib` in `Libs:`), but the `pkg_config`
///   crate forwards only `-L` and `-l` and drops the rest.
/// - Cargo scopes a build script's `rustc-link-arg` to its own package's targets, so
///   `cortex`'s build script cannot supply it for this binary.
/// - `DYLD_FALLBACK_LIBRARY_PATH` would paper over it, and cannot be relied on: macOS strips
///   every `DYLD_*` variable when it executes a system binary, so anything reached through
///   `/bin/sh` loses it.
///
/// Without it the binary aborts in dyld before it reaches `main` — `Library not loaded:
/// @rpath/libfuse-t.dylib … no LC_RPATH's found` — and a caller sees a console server that
/// closed the channel before answering anything.
///
/// Unconditional, because the feature that decides whether the dylib is linked is not this
/// crate's to read. A probe that finds nothing is a host without FUSE-T, where `cortex/fuse-t`
/// cannot have been built either and no rpath is wanted; a probe that finds it on a build that
/// did not link the dylib leaves one unused load command, which costs nothing.
fn fuse_t_rpath() {
    // Metadata off: this asks *where* FUSE-T is, not to link it. `probe` would otherwise
    // print the `-l fuse-t` that pulls the dylib into a binary that had no reason to carry it.
    let Ok(fuse_t) = pkg_config::Config::new().cargo_metadata(false).probe("fuse-t") else {
        return;
    };
    for path in &fuse_t.link_paths {
        println!("cargo::rustc-link-arg-bins=-Wl,-rpath,{}", path.display());
    }
}

/// Cross-compile the guest half and leave it in `OUT_DIR`.
fn guest() -> anyhow::Result<()> {
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

    // A libkrun guest runs the host's architecture: there is no emulation in the middle
    // to make anything else work.
    let target = match std::env::consts::ARCH {
        "aarch64" => "aarch64-unknown-linux-musl",
        "x86_64" => "x86_64-unknown-linux-musl",
        other => anyhow::bail!("no guest target for a {other} host"),
    };

    let built = build(
        &crate_dir,
        "guest",
        &["build", "--release", "--target", target],
        &format!("{target}/release/cortex-uvm-guest"),
        &format!(
            "building cortex-uvm-guest for {target} failed.\n\
             If the target is not installed: rustup target add {target}"
        ),
    )?;
    place(&built, &out)
}

/// Build the boot half for *this* host and leave it in `OUT_DIR`.
///
/// The host's own target, unlike the guest's — a boot runs here, next to the hypervisor it
/// calls. What it does share with the guest is being built by a cargo of its own: the `vm`
/// feature drags in a VMM that has no business in this crate's dependency graph, and a target
/// directory of its own is what keeps the two builds from waiting on each other's lock.
///
/// `--release` because these bytes are embedded and then copied and signed on the way to every
/// boot, so their size is paid more than once.
fn boot() -> anyhow::Result<()> {
    let out = PathBuf::from(std::env::var("OUT_DIR")?).join("cortex-uvm-boot");

    println!("cargo::rerun-if-env-changed=CORTEX_UVM_BOOT_BIN");
    if let Some(prebuilt) = std::env::var_os("CORTEX_UVM_BOOT_BIN") {
        let prebuilt = PathBuf::from(prebuilt);
        println!("cargo::rerun-if-changed={}", prebuilt.display());
        std::fs::copy(&prebuilt, &out)
            .map_err(|e| anyhow::anyhow!("CORTEX_UVM_BOOT_BIN={}: {e}", prebuilt.display()))?;
        return Ok(());
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

    let built = build(
        &crate_dir,
        "boot",
        &[
            "build",
            "--release",
            "--features",
            "vm",
            "--bin",
            "cortex-uvm-boot",
        ],
        "release/cortex-uvm-boot",
        "building cortex-uvm-boot failed",
    )?;
    place(&built, &out)
}

fn place(built: &Path, out: &Path) -> anyhow::Result<()> {
    std::fs::copy(built, out)
        .map_err(|e| anyhow::anyhow!("copying {} to {}: {e}", built.display(), out.display()))?;
    Ok(())
}

/// Run a nested cargo in `crate_dir` with a target directory of its own, and say where the
/// binary landed.
fn build(
    crate_dir: &Path,
    target_subdir: &str,
    args: &[&str],
    built: &str,
    failed: &str,
) -> anyhow::Result<PathBuf> {
    let target_dir = PathBuf::from(std::env::var("OUT_DIR")?).join(target_subdir);

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        // From the crate's own directory, so its `.cargo/config.toml` — where it has one — is
        // the configuration in effect.
        .current_dir(crate_dir)
        .args(args)
        .env("CARGO_TARGET_DIR", &target_dir)
        // This build's choices are for this build's target.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .status()
        .map_err(|e| anyhow::anyhow!("running cargo in {}: {e}", crate_dir.display()))?;

    anyhow::ensure!(status.success(), "{failed}");
    Ok(target_dir.join(built))
}
