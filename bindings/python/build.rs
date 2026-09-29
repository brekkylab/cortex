//! Delay-loads `dokan2.dll` when the `mount` feature is on for a Windows MSVC target.
//!
//! cortex's `build.rs` asks for the same, but `rustc-link-arg` applies only to the printing
//! package's targets, so this cdylib must ask again. Without it the import fails in the loader
//! with `DLL load failed` on hosts without Dokany, even for callers that never mount; with it
//! the DLL loads at the first mount, which `cortex::fs::mount_support` checks for first.
//!
//! macOS needs nothing: the FUSE-T shim opens libfuse-t itself at run time.
fn main() {
    if std::env::var_os("CARGO_FEATURE_MOUNT").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo::rustc-link-arg=/DELAYLOAD:dokan2.dll");
        println!("cargo::rustc-link-lib=delayimp");
    }
}
