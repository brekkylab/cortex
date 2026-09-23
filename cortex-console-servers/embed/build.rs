//! Build the console servers this crate's features ask for, and leave each in `OUT_DIR` as a
//! zstd frame and the constant that names it.
//!
//! | feature | server | what it needs to build |
//! |---|---|---|
//! | `local` | `cortex-local-console` | nothing beyond this toolchain |
//! | `uvm` | `cortex-uvm-console` | the `<arch>-unknown-linux-musl` target, for its guest half |
//!
//! # One cargo of its own, and why
//!
//! The servers depend on `cortex`, and `cortex` depends on this crate — so they cannot be a
//! dependency of it, and are built instead by a cargo this script starts. That cargo is outside
//! the graph being built, which is what makes it not a cycle; `cortex-uvm-console` builds its
//! guest and its boot half the same way and for a related reason.
//!
//! What that cargo is given, and what it is not:
//!
//! - **The target this crate is being built for** (`TARGET`), not the host. What is embedded
//!   runs wherever the binary that carries it runs, which is a cross build's target.
//! - **Always `--release`,** whatever profile the outer build is. The bytes are paid for in every
//!   binary that carries them, and a debug server is several times the size for no benefit to
//!   anybody running it. The profile it gets is [`PROFILE`], passed as `--config` so that no
//!   `[profile]` in any manifest has to change.
//! - **A target directory of its own**, under `OUT_DIR`. A shared one would deadlock on cargo's
//!   own lock.
//! - **None of the outer build's configuration.** `RUSTFLAGS`, wrappers, and every
//!   `CARGO_PROFILE_*`: those were chosen for the outer build's target and profile, and a
//!   `CARGO_PROFILE_*` in particular is cargo *configuration*, which outranks a manifest's
//!   `[profile]` — left in, it would reach the guest `cortex-uvm-console` builds and override the
//!   `opt-level = "z"` that crate's own manifest asks for.
//! - **`--locked`.** The servers are built against the lockfile they were tested with, and a
//!   checkout this is built from — a git dependency's — is not somewhere to write a new one.
//!
//! # Variables
//!
//! - `CORTEX_EMBED_LOCAL_CONSOLE_BIN`, `CORTEX_EMBED_UVM_CONSOLE_BIN`: embed the file named
//!   rather than building one. For a caller who builds the server some other way, and for a
//!   machine that cannot build the guest.
//! - `CORTEX_EMBED_FAST`: thin LTO and parallel codegen instead of the release profile below.
//!   Minutes faster, a few megabytes larger — for working on cortex itself, where every change
//!   to it rebuilds the servers.
//!
//! [`PROFILE`]: PROFILE

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

/// Set on the cargo this starts. Nothing it builds should reach this script again — the
/// servers take `cortex` without the features that pull this crate in — so seeing it here is a
/// configuration that would recurse, and is said rather than followed.
const NESTED_ENV: &str = "CORTEX_EMBED_NESTED";

const FAST_ENV: &str = "CORTEX_EMBED_FAST";

/// How hard to compress. The encode is paid once per build of a server; the decode, once per
/// machine per build — and zstd's decode speed barely moves with the level, so the highest
/// level short of `--ultra` is the one to pay for.
const LEVEL: i32 = 19;

/// The release profile the servers are built with.
///
/// Fat LTO and one codegen unit are most of the size — measured on `cortex-uvm-console`,
/// 21.7 MB stripped against 15.2 MB — and cost nothing at run time. `opt-level` stays 3: `z`
/// makes the binary a third smaller, but once it is compressed the difference is under a
/// megabyte, and it halves how fast the server hashes a layer.
const PROFILE: &[(&str, &str)] = &[
    ("opt-level", "3"),
    ("lto", "\"fat\""),
    ("codegen-units", "1"),
    ("strip", "\"symbols\""),
];

/// What [`FAST_ENV`] builds with instead.
const FAST_PROFILE: &[(&str, &str)] = &[
    ("opt-level", "3"),
    ("lto", "\"thin\""),
    ("codegen-units", "16"),
    ("strip", "\"symbols\""),
];

struct Server {
    /// The feature, as cargo spells it to a build script: `CARGO_FEATURE_<this>`.
    feature: &'static str,
    /// Its package, which is also its binary's name.
    package: &'static str,
    /// The constant `lib.rs` exports it as.
    constant: &'static str,
    /// Where it lives in the workspace, from the root.
    dir: &'static str,
    /// Paths under the workspace root whose change is a change to this server, beyond its own
    /// directory and `cortex`.
    also: &'static [&'static str],
    /// Variables its own build reads, so a change to one rebuilds it.
    envs: &'static [&'static str],
}

const SERVERS: &[Server] = &[
    Server {
        feature: "LOCAL",
        package: "cortex-local-console",
        constant: "LOCAL",
        dir: "cortex-console-servers/local",
        also: &[],
        envs: &[],
    },
    Server {
        feature: "UVM",
        package: "cortex-uvm-console",
        constant: "UVM",
        dir: "cortex-console-servers/uvm/console",
        // Its two embedded halves. The guest is a workspace of its own, with its own lockfile.
        also: &[
            "cortex-console-servers/uvm/boot/src",
            "cortex-console-servers/uvm/boot/Cargo.toml",
            "cortex-console-servers/uvm/guest/src",
            "cortex-console-servers/uvm/guest/Cargo.toml",
            "cortex-console-servers/uvm/guest/Cargo.lock",
            "cortex-console-servers/uvm/guest/.cargo",
        ],
        envs: &["CORTEX_UVM_GUEST_BIN", "CORTEX_UVM_BOOT_BIN"],
    },
];

fn main() -> anyhow::Result<()> {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed={NESTED_ENV}");
    anyhow::ensure!(
        env::var_os(NESTED_ENV).is_none(),
        "cortex-console-embed is being built by the cargo its own build script started — \
         something in that build enabled cortex's `local` or `uvm` feature, and would recurse"
    );

    let out = PathBuf::from(env::var("OUT_DIR")?);
    let wanted: Vec<&Server> = SERVERS
        .iter()
        .filter(|server| env::var_os(format!("CARGO_FEATURE_{}", server.feature)).is_some())
        .collect();

    // Prebuilt ones first, so that a caller who stood in every server starts no cargo at all.
    let mut binaries = Vec::new();
    let mut to_build = Vec::new();
    for server in wanted {
        let var = prebuilt_env(server);
        println!("cargo::rerun-if-env-changed={var}");
        match env::var_os(&var) {
            Some(path) => {
                let path = PathBuf::from(path);
                println!("cargo::rerun-if-changed={}", path.display());
                binaries.push((server, path));
            }
            None => to_build.push(server),
        }
    }
    if !to_build.is_empty() {
        binaries.extend(build(&to_build, &out)?);
    }

    let mut generated = String::new();
    for (server, binary) in binaries {
        let bytes = std::fs::read(&binary)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", binary.display()))?;
        let digest = {
            use sha2::{Digest as _, Sha256};
            format!("{:x}", Sha256::digest(&bytes))
        };
        let compressed = zstd::bulk::compress(&bytes, LEVEL)?;
        let file = format!("{}.zst", server.package);
        std::fs::write(out.join(&file), compressed)?;

        generated.push_str(&format!(
            "/// `{package}`, built for this crate's target.\n\
             pub const {constant}: Embedded = Embedded {{\n    \
                 name: {package:?},\n    \
                 digest: {digest:?},\n    \
                 len: {len},\n    \
                 zstd: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{file}\")),\n\
             }};\n",
            package = server.package,
            constant = server.constant,
            len = bytes.len(),
        ));
    }
    std::fs::write(out.join("embedded.rs"), generated)?;
    Ok(())
}

fn prebuilt_env(server: &Server) -> String {
    format!(
        "CORTEX_EMBED_{}_BIN",
        server
            .package
            .trim_start_matches("cortex-")
            .replace('-', "_")
            .to_uppercase()
    )
}

/// Build `servers` with one cargo, and say where each binary landed.
///
/// One invocation rather than one per server, because the two share `cortex` and everything
/// under it: built together, that is compiled once.
fn build<'a>(servers: &[&'a Server], out: &Path) -> anyhow::Result<Vec<(&'a Server, PathBuf)>> {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?)
        .join("../..")
        .canonicalize()?;

    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "cortex/src",
        "cortex/Cargo.toml",
        "cortex/build.rs",
    ] {
        println!("cargo::rerun-if-changed={}", root.join(path).display());
    }
    for server in servers {
        for path in ["src", "Cargo.toml", "build.rs"] {
            println!(
                "cargo::rerun-if-changed={}",
                root.join(server.dir).join(path).display()
            );
        }
        for path in server.also {
            println!("cargo::rerun-if-changed={}", root.join(path).display());
        }
        for var in server.envs {
            println!("cargo::rerun-if-env-changed={var}");
        }
    }
    println!("cargo::rerun-if-env-changed={FAST_ENV}");

    let target = env::var("TARGET")?;
    let target_dir = out.join("target");
    let profile = if env::var_os(FAST_ENV).is_some() {
        FAST_PROFILE
    } else {
        PROFILE
    };

    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(cargo);
    command
        .current_dir(&root)
        .args(["build", "--release", "--locked", "--target", &target]);
    for server in servers {
        command.args(["-p", server.package, "--bin", server.package]);
    }
    for (key, value) in profile {
        command
            .arg("--config")
            .arg(format!("profile.release.{key}={value}"));
    }
    command
        .env("CARGO_TARGET_DIR", &target_dir)
        .env(NESTED_ENV, "1");
    scrub(&mut command);

    let status = command
        .status()
        .map_err(|e| anyhow::anyhow!("running cargo in {}: {e}", root.display()))?;
    if !status.success() {
        let names: Vec<_> = servers.iter().map(|s| s.package).collect();
        let mut message = format!(
            "building {} for {target} failed ({status})",
            names.join(", ")
        );
        if servers.iter().any(|s| s.feature == "UVM") {
            message.push_str(
                "\n`cortex-uvm-console` needs the musl target for its guest — if that is what \
                 failed: rustup target add <arch>-unknown-linux-musl. \
                 CORTEX_EMBED_UVM_CONSOLE_BIN embeds a server built some other way.",
            );
        }
        anyhow::bail!(message);
    }

    Ok(servers
        .iter()
        .map(|server| {
            let binary = target_dir
                .join(&target)
                .join("release")
                .join(server.package);
            (*server, binary)
        })
        .collect())
}

/// Take out of `command` what was chosen for the build that is running this script.
///
/// See the module documentation for why each of these. `CARGO_FEATURE_*` and `CARGO_CFG_*` are
/// this script's own and would be read by the nested build's scripts as theirs — `cortex`'s
/// reads `CARGO_FEATURE_FUSE_T`.
fn scrub(command: &mut Command) {
    for var in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        command.env_remove(var);
    }
    for (var, _) in env::vars_os() {
        let Some(name) = var.to_str() else { continue };
        if name.starts_with("CARGO_PROFILE_")
            || name.starts_with("CARGO_FEATURE_")
            || name.starts_with("CARGO_CFG_")
        {
            command.env_remove(&var);
        }
    }
}
