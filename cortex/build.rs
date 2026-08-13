//! Compiles the FUSE-T shim when the `fuse-t` feature is on. See
//! `contrib/fuse_t/shim.h` for why the shim exists.

fn main() {
    println!("cargo::rerun-if-changed=contrib/fuse_t/shim.c");
    println!("cargo::rerun-if-changed=contrib/fuse_t/shim.h");

    if std::env::var_os("CARGO_FEATURE_FUSE_T").is_none() {
        return;
    }

    // FUSE-T ships `fuse-t.pc`. Not the `fuse.pc` shim in `contrib/pkgconfig`,
    // which exists only to satisfy `fuser`'s macFUSE probe.
    let fuse_t = pkg_config::Config::new()
        .probe("fuse-t")
        .expect("the `fuse-t` feature needs FUSE-T installed: brew install --cask fuse-t");

    // `libfuse-t.dylib`'s install name is `@rpath/libfuse-t.dylib`, so a linking binary needs
    // an `LC_RPATH`. `fuse-t.pc` asks for one in its `Libs:`, but `pkg_config::probe` forwards
    // only `-L` and `-l`.
    //
    // Without it the binary loads only when `DYLD_FALLBACK_LIBRARY_PATH` is set, which is not
    // something to rely on: macOS strips every `DYLD_*` when it execs a system binary, so a
    // command run through `/bin/sh` — how a console runs anything — loses it. Measured with
    // the variable exported, `sh -c 'echo "[$DYLD_…]"'` prints `[]`.
    for path in &fuse_t.link_paths {
        println!("cargo::rustc-link-arg=-Wl,-rpath,{}", path.display());
    }

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
    build.compile("cortex_fuse_t_shim");
}
