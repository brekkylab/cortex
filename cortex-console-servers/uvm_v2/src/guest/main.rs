//! `cortex-uvm-v2-guest` — the half that runs inside the guest.
//!
//! A skeleton: it exists, builds, and runs. Nothing of the protocol is here yet.
//!
//! Built for the guest's target rather than this host's, so anything it takes has to be
//! buildable there — which is the reason it is its own binary and not a flag on the host's.

fn main() {
    println!("hello world from the guest");
}
