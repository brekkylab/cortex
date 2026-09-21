//! The host-call channel, exercised once: a session boots, and something inside it dials out.
//!
//! ```sh
//! scripts/build-abin.sh                       # `/abin`, this host's architecture
//! cp -R target/abin "${CORTEX_UVM_HOME:-$HOME/.cache/cortex}/abin"
//! cargo run -p cortex-uvm-v2-host --example hostcall
//! ```
//!
//! What is being answered is the one question the design rests on and the host cannot ask
//! itself: **does a process inside the guest reach this host?** That needs a vsock driver in
//! the guest kernel, a port the boot attached, and the socket this server bound behind it —
//! and the only vantage point from which all three are visible at once is a program in the
//! guest. That program is `infer`, and this example is what runs it.
//!
//! A run that prints `connected:` is the channel proven end to end. The line after it is the
//! frame, sent at a handler that is still a `todo!()` — so the connection closing without an
//! answer is the expected reading, and the panic behind it appears on this server's own
//! stderr. See `infer`'s own docs for why the stages are reported apart.
//!
//! # What it needs
//!
//! A libkrunfw to boot against — `brew install libkrunfw`, or microsandbox's copy — a network
//! the first time, for the registry, and an `/abin` under this server's home: `infer` is one
//! of the executables `scripts/build-abin.sh` builds, and a session gets none unless that
//! directory is there.

use std::path::PathBuf;

use cortex::{
    console::{Console, NetworkAccess},
    rootfs_v2::RootFsV2,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let host = host()?;

    // Said before a machine is booted, because a session with no `/abin` has nothing to run
    // here and the failure would otherwise arrive as a command that was not found — a minute
    // and a registry pull later, saying nothing about which directory was missing.
    let probe = home().join("abin/infer");
    anyhow::ensure!(
        probe.is_file(),
        "no infer at {} — build `/abin` (`scripts/build-abin.sh`) and put it there, which is \
         where a session's `/abin` is mounted from",
        probe.display()
    );

    // The plainest base there is: what is being exercised is underneath the session rather
    // than in it, and `infer` is static and needs nothing from the image. No network, which
    // is also the point — the host-call channel is not the network, and a session that reaches
    // nothing still reaches this.
    let mut console = Console::builder()
        .stdio_client(&[host.to_string_lossy().as_ref()])
        .rootfs(RootFsV2::new().base("alpine:3.20"))
        .network(NetworkAccess::none())
        .build()
        .await?;

    let resp = console.exec(["infer"], None).await?;
    print!("{}", String::from_utf8_lossy(&resp.stdout));
    print!("{}", String::from_utf8_lossy(&resp.stderr));

    // A non-zero code is the probe saying which of the three stages it did not get past, which
    // it has already printed. What is added here is that the run failed, so this is not an
    // example anybody reads as having passed.
    anyhow::ensure!(
        resp.code == 0,
        "infer exited with {} — the host-call channel did not come up",
        resp.code
    );
    Ok(())
}

/// Where the server keeps what it has, by the same rule the server itself reads — its own
/// `home`, which is not public and is two lines.
fn home() -> PathBuf {
    std::env::var_os("CORTEX_UVM_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/cortex")))
        .expect("neither CORTEX_UVM_HOME nor HOME is set")
}

/// The server this example drives: `$CORTEX_UVM_V2_HOST_BIN`, else the binary one directory
/// up from this one — the rule `hello` follows, for the reason it gives.
fn host() -> anyhow::Result<PathBuf> {
    if let Some(named) = std::env::var_os("CORTEX_UVM_V2_HOST_BIN") {
        return Ok(PathBuf::from(named));
    }

    let exe = std::env::current_exe()?;
    let host = exe
        .parent()
        .and_then(|examples| examples.parent())
        .ok_or_else(|| anyhow::anyhow!("{} has nowhere to look up from", exe.display()))?
        .join("cortex-uvm-v2-host");
    anyhow::ensure!(
        host.is_file(),
        "no cortex-uvm-v2-host at {} — `cargo build -p cortex-uvm-v2-host`, or name one in \
         CORTEX_UVM_V2_HOST_BIN",
        host.display()
    );
    Ok(host)
}
