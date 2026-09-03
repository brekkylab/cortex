//! Gives this binary an `LC_RPATH` for FUSE-T, which nothing else can.
//!
//! `libfuse-t.dylib`'s install name is `@rpath/libfuse-t.dylib`, so anything linking it
//! needs an rpath saying where that is. Three things conspire to make this the only place
//! it can come from:
//!
//! - `fuse-t.pc` asks for it (`-Wl,-rpath,/usr/local/lib` in `Libs:`), but the
//!   `pkg_config` crate forwards only `-L` and `-l` and drops the rest.
//! - Cargo scopes a build script's `rustc-link-arg` to its own package's targets, so
//!   `cortex`'s build script cannot supply it for this binary. Measured: adding it there
//!   leaves this binary with no `LC_RPATH` at all.
//! - `DYLD_FALLBACK_LIBRARY_PATH` would paper over it, and cannot be relied on: macOS
//!   strips every `DYLD_*` variable when it executes a system binary, so anything reached
//!   through `/bin/sh` loses it.
//!
//! Without this, a build with the `fuse-t` feature aborts in dyld before it reaches
//! `main`.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_FEATURE_FUSE_T").is_none() {
        return;
    }

    // Already probed and found by `cortex`'s build script — this feature forwards to
    // `cortex/fuse-t` — so a failure here means something changed underneath that build.
    let fuse_t = pkg_config::Config::new()
        .probe("fuse-t")
        .expect("the `fuse-t` feature needs FUSE-T installed: brew install --cask fuse-t");

    for path in &fuse_t.link_paths {
        println!("cargo::rustc-link-arg-bins=-Wl,-rpath,{}", path.display());
    }
}
