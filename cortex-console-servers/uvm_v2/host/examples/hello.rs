//! The whole of this server, exercised once: a rootfs declared, and something in it run.
//!
//! ```sh
//! cargo run -p cortex-uvm-v2-host --example hello
//! ```
//!
//! One call opens the session, and everything underneath it is what is being tested: the
//! base is pulled, the `RUN` is a command in a micro-VM of its own, the layer it leaves is
//! committed out of that guest's upperdir, and the session then boots on the stack of both.
//! So a `hello world` here means the boot half, the guest half, the registry client and the
//! layer store all worked — and `python3` being *found* means the step over the base was
//! actually realized, which is the failure worth catching.
//!
//! The second run is the other half of the point. A `RUN` is remembered under the image and
//! command asked for, so it does not boot a machine again, and the image is the same digest
//! it was the first time. What that costs on a cold cache is a pull and a boot; afterwards it
//! is a file test.
//!
//! # What it needs
//!
//! A libkrunfw to boot against — `brew install libkrunfw`, or microsandbox's copy — and a
//! network, the first time, for the registry and for `apk`. Point `CORTEX_UVM_HOME` somewhere
//! disposable for a run that starts cold.

use std::path::PathBuf;

use cortex::{
    console::{Console, NetworkAccess},
    rootfs_v2::RootFsV2,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let host = host()?;

    // The declaration, handed over whole: the server builds it and boots the session on what
    // came out, so there is nothing for this end to build first and no digest to carry around.
    //
    // No network. The `apk add` is the build's and runs with one; the session over it needs
    // none, and a session that reaches nothing is what this server is for.
    let mut console = Console::builder()
        .stdio_client(&[host.to_string_lossy().as_ref()])
        .rootfs(
            RootFsV2::new()
                .base("alpine:3.20")
                .step("apk add --no-cache python3"),
        )
        .network(NetworkAccess::none())
        .build()
        .await?;

    let resp = console
        .exec(["python3", "-c", "print('hello world')"], None)
        .await?;

    // What python wrote, as python wrote it — the point of the example is the string, and a
    // `Vec<u8>` printed as numbers is not it.
    print!("stdout: {}", String::from_utf8_lossy(&resp.stdout));
    print!("stderr: {}", String::from_utf8_lossy(&resp.stderr));
    anyhow::ensure!(resp.code == 0, "python exited with {}", resp.code);

    Ok(())
}

/// The server this example drives: `$CORTEX_UVM_V2_HOST_BIN`, else the binary one directory
/// up from this one.
///
/// Up rather than beside, because cargo puts an example in `examples/` under the profile
/// directory and the binaries next to that — so this is the same rule `session::halves`
/// follows for the other two halves, counted from where an example lands.
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
