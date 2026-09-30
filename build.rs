//! What the `mount` feature needs from the target's filesystem provider at build time: on
//! macOS the FUSE-T shim, compiled; on Windows the Dokany DLL, delay-loaded. See
//! `contrib/fuse_t/shim.h` for why the shim exists, and `src/fs/mount/support.rs` for why
//! neither provider is a dependency a binary needs in order to start.

fn main() {
    println!("cargo::rerun-if-changed=contrib/fuse_t/shim.c");
    println!("cargo::rerun-if-changed=contrib/fuse_t/shim.h");

    if std::env::var_os("CARGO_FEATURE_MOUNT").is_none() {
        return;
    }
    // The target's, not `cfg!(target_os)`: a build script is compiled for the host.
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => fuse_t_shim(),
        Ok("windows") => delay_load_dokan(),
        _ => {}
    }
}

/// Compile the shim against FUSE-T's headers. **Headers only**: the shim opens libfuse-t
/// with `dlopen` when a mount first asks for it, so nothing is linked and nothing needs an
/// rpath -- which is also why a dependent's own link needs nothing from this.
fn fuse_t_shim() {
    let fuse_t = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("fuse-t")
        .expect("the `mount` feature on macOS builds against FUSE-T's headers: brew install --cask fuse-t");

    let mut build = cc::Build::new();
    build
        .file("contrib/fuse_t/shim.c")
        .include("contrib/fuse_t")
        // libfuse's headers pick struct layouts off this; a mismatch is a silent
        // ABI break.
        .define("_FILE_OFFSET_BITS", "64")
        .warnings(true);
    for path in &fuse_t.include_paths {
        build.include(path);
    }
    // Where to look when the loader does not find the library by its bare name.
    if let Some(dir) = fuse_t.link_paths.first() {
        build.define(
            "VIRTX_FUSE_T_LIBDIR",
            format!("\"{}\"", dir.display()).as_str(),
        );
    }
    build.compile("virtx_fuse_t_shim");
}

/// Load `dokan2.dll` at its first call rather than at process start, for this package's
/// own tests and examples. A dependent's binary has to ask for the same from its own
/// `build.rs` -- a `rustc-link-arg` reaches only the package that prints it -- as the
/// bindings' do. `/DELAYLOAD` is MSVC's; a GNU target links the DLL at start as before.
fn delay_load_dokan() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo::rustc-link-arg=/DELAYLOAD:dokan2.dll");
        println!("cargo::rustc-link-lib=delayimp");
    }
}
