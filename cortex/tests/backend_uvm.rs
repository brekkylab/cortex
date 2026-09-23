//! `Backend::uvm`: a micro-VM console whose server nobody installed.
//!
//! Ignored by default, because it boots a guest — a hypervisor, a libkrunfw, and on a cold
//! cache a base image and `/abin` to fetch. It uses `~/.cortex` like any caller would, so a
//! machine that has run a uvm session before has all of that already:
//!
//!   cargo test -p cortex --features uvm --test backend_uvm -- --ignored
//!
//! `CORTEX_UVM_KERNEL` names a libkrunfw to boot instead of the pinned one the server fetches.
#![cfg(feature = "uvm")]

use cortex::console::{Backend, Console};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn a_uvm_console_runs_without_a_program_being_named() {
    let mut console = Console::builder()
        .backend(Backend::uvm().vcpus(1).memory_mib(512))
        .build()
        .await
        .unwrap();

    let result = console
        .exec(
            ["sh", "-c", "uname -s; nproc; grep MemTotal /proc/meminfo"],
            None,
        )
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some("Linux"), "{stdout}");
    // The typed options are what the guest was booted with.
    assert_eq!(lines.next(), Some("1"), "{stdout}");
    let kib: u64 = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|kib| kib.parse().ok())
        .unwrap_or_else(|| panic!("no MemTotal in {stdout}"));
    assert!(
        kib <= 512 * 1024,
        "{kib} KiB is more than the 512 MiB asked for"
    );
}
