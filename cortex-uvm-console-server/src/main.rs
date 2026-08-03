//! `cortex-uvm-console-server` — entry point for the krun micro-VM console-server backend.
//!
//! Stub: prints and exits.
//!
//! Note: once this actually boots a VM it must carry the
//! `com.apple.security.hypervisor` entitlement on macOS — entitlements come from
//! the main executable, so this binary is the one to codesign.

fn main() {
    println!("hello world from cortex-uvm-console-server");
}
