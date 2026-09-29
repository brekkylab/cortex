//! napi's link setup, plus delay-loading `dokan2.dll` for Windows MSVC with the `mount` feature.
//!
//! cortex's `build.rs` asks for the delay-load too, but `rustc-link-arg` applies only to the
//! printing package's targets, so this cdylib must ask again. Without it `require` fails in the
//! loader on hosts without Dokany, even for callers that never mount; with it the DLL loads at
//! the first mount, which `cortex::fs::mount_support` checks for first.
//!
//! macOS needs nothing: the FUSE-T shim opens libfuse-t itself at run time.
fn main() {
    napi_build::setup();
    if std::env::var_os("CARGO_FEATURE_MOUNT").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo::rustc-link-arg=/DELAYLOAD:dokan2.dll");
        println!("cargo::rustc-link-lib=delayimp");
    }
}
