//! A session carried on in another one: what one wrote, taken as a snapshot and handed to the
//! next.
//!
//! ```sh
//! cargo run -p cortex-uvm-v2-host --example snapshot
//! ```
//!
//! Three things have to survive the trip, and they are the three this checks. A **file the
//! session wrote** is the easy one. A **file it deleted out of the base** is the one an
//! ordinary archive loses: overlayfs records a deletion as a device node, and a snapshot that
//! carried the tree rather than the *changes* to it would hand back a session where `rm` had
//! quietly not happened. A **directory it emptied** is the same problem once more, marked with
//! an xattr no tar carries.
//!
//! So the second session is asked what it sees, and the answer is the first session's.
//!
//! # What it needs
//!
//! The same as `hello`: a libkrunfw to boot against, and a network the first time, for the
//! registry.

use std::path::PathBuf;

use cortex::{
    console::{Console, NetworkAccess},
    rootfs_v2::RootFsV2,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let host = host()?;

    // Plain alpine, and no step over it: what is being tested is what a *session* leaves, so
    // a build on top would only put the same files there by another route.
    let rootfs = RootFsV2::new().base("alpine:3.20");

    let blob = {
        let mut first = Console::builder()
            .stdio_client(&[host.to_string_lossy().as_ref()])
            .rootfs(rootfs.clone())
            .network(NetworkAccess::none())
            .build()
            .await?;

        // One of each. `/media` is emptied by being replaced rather than by having its
        // contents removed one at a time, which is what makes overlayfs mark the directory
        // opaque instead of writing a whiteout per child — the case a tar cannot carry as
        // itself.
        run(
            &mut first,
            "echo carried > /root/kept && rm /etc/hostname && rm -rf /media && mkdir /media",
        )
        .await?;

        // Taken before the session is dropped, which is what makes the bytes worth having:
        // dropping it takes the machine down and the disk with it.
        let blob = first.snapshot().await?;
        anyhow::ensure!(!blob.is_empty(), "the snapshot came back empty");
        println!("snapshot: {} bytes", blob.len());
        blob
    };

    // A second session on the same base, told to start where the first stopped.
    let mut second = Console::builder()
        .stdio_client(&[host.to_string_lossy().as_ref()])
        .rootfs(rootfs)
        .network(NetworkAccess::none())
        .snapshot(blob)
        .build()
        .await?;

    let kept = run(&mut second, "cat /root/kept").await?;
    anyhow::ensure!(
        kept.trim() == "carried",
        "the file the first session wrote came back as {kept:?}"
    );
    println!("written file: carried over");

    // `test -e` and not `cat`: what is being asked is whether the base's copy is visible
    // again, and a deletion that did not survive shows up as the file being *there*.
    let gone = run(
        &mut second,
        "test -e /etc/hostname && echo there || echo gone",
    )
    .await?;
    anyhow::ensure!(
        gone.trim() == "gone",
        "the file the first session deleted is {gone:?} in the second"
    );
    println!("deleted file: still deleted");

    // The base ships `/media` with `cdrom`, `floppy` and `usb` in it. A directory that came
    // back opaque is empty; one that came back as a plain directory shows those again.
    let emptied = run(&mut second, "ls -A /media | wc -l").await?;
    anyhow::ensure!(
        emptied.trim() == "0",
        "the directory the first session emptied has {} entries in the second",
        emptied.trim()
    );
    println!("emptied directory: still empty");

    Ok(())
}

/// Run one command through a session and hand back its stdout, refusing anything but success.
///
/// A shell, because every command here is a sequence: `sh -c` is what this protocol offers for
/// that and what the example would otherwise spell three times.
async fn run(console: &mut Console, script: &str) -> anyhow::Result<String> {
    let resp = console.exec(["sh", "-c", script], None).await?;
    anyhow::ensure!(
        resp.code == 0,
        "{script:?} exited with {}:\n{}{}",
        resp.code,
        String::from_utf8_lossy(&resp.stdout),
        String::from_utf8_lossy(&resp.stderr)
    );
    Ok(String::from_utf8_lossy(&resp.stdout).into_owned())
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
