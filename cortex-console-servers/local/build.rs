//! Gives this binary an `LC_RPATH` for FUSE-T, which nothing else can.
//!
//! `libfuse-t.dylib`'s install name is `@rpath/libfuse-t.dylib`, so anything linking it
//! needs an rpath saying where that is. This binary links it without ever asking to:
//! `cortex/fuse-t` is a feature of a *dependency*, and a workspace build that enables it
//! anywhere — `cargo build --all-features`, a sibling member, a test — unifies it into the
//! cortex this links. Three things then conspire to make this the only place the rpath can
//! come from:
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
//! Without this the binary aborts in dyld before it reaches `main` — `Library not loaded:
//! @rpath/libfuse-t.dylib … no LC_RPATH's found` — and a caller sees a console server that
//! closed the channel before answering anything.
//!
//! Unconditional, because the feature that decides whether the dylib is linked is not this
//! crate's to read: this crate has no `fuse-t` feature of its own, so the guard that used to
//! stand here — `CARGO_FEATURE_FUSE_T` — was never set and the rpath was never written. A
//! probe that finds nothing is a host without FUSE-T, where `cortex/fuse-t` cannot have been
//! built either and no rpath is wanted; a probe that finds it on a build that did not link
//! the dylib leaves one unused load command, which costs nothing.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    // Metadata off: this asks *where* FUSE-T is, not to link it. `probe` would otherwise
    // print the `-l fuse-t` that pulls the dylib into a binary that had no reason to carry
    // it.
    let Ok(fuse_t) = pkg_config::Config::new().cargo_metadata(false).probe("fuse-t") else {
        return;
    };

    for path in &fuse_t.link_paths {
        println!("cargo::rustc-link-arg-bins=-Wl,-rpath,{}", path.display());
    }
}
