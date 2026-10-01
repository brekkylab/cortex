//! What a binary that takes cortex with `mount` needs on Windows, as the README says: the
//! delay-load of `dokan2.dll`, which cortex's own `build.rs` cannot ask for on a dependent's
//! behalf. Without it this binary would not start on a host without Dokany.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo::rustc-link-arg=/DELAYLOAD:dokan2.dll");
        println!("cargo::rustc-link-lib=delayimp");
    }
}
