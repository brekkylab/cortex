//! Build-time needs of the `mount` feature: on macOS, compile the FUSE-T shim; on Windows,
//! delay-load the Dokany DLL. Neither provider is needed for a binary to start (see
//! `src/fs/mount/support.rs`; `contrib/fuse_t/shim.h` explains the shim).

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

/// Compile the shim against FUSE-T's headers only: it `dlopen`s libfuse-t on first mount, so
/// nothing is linked, no rpath is needed, and a dependent's link needs nothing from this.
fn fuse_t_shim() {
    let fuse_t = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("fuse-t")
        .expect("the `mount` feature on macOS builds against FUSE-T's headers: brew install --cask fuse-t");

    let mut build = cc::Build::new();
    build
        .file("contrib/fuse_t/shim.c")
        .include("contrib/fuse_t")
        // libfuse's headers pick struct layouts off this; a mismatch is a silent ABI break.
        .define("_FILE_OFFSET_BITS", "64")
        .warnings(true);
    for path in &fuse_t.include_paths {
        build.include(path);
    }
    // Where to look when the loader does not find the library by its bare name.
    if let Some(dir) = fuse_t.link_paths.first() {
        build.define(
            "CORTEX_FUSE_T_LIBDIR",
            format!("\"{}\"", dir.display()).as_str(),
        );
    }
    build.compile("cortex_fuse_t_shim");
}

/// Load `dokan2.dll` at first call rather than process start, for this package's own tests
/// and examples. A `rustc-link-arg` reaches only the package printing it, so a dependent's
/// binary must request the same in its own `build.rs`. `/DELAYLOAD` is MSVC-only; a GNU
/// target links the DLL at start.
fn delay_load_dokan() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo::rustc-link-arg=/DELAYLOAD:dokan2.dll");
        println!("cargo::rustc-link-lib=delayimp");
    }
}
