//! `/abin` in a running guest: are the executables there, do they run, is the disk really
//! read-only, and is it first on `PATH`?
//!
//! **The executables here are shell scripts.** What this tests is the delivery — the disk,
//! the mount, the ordering, the refusals — and a script exercises all of it without making
//! the test depend on a musl cross-compiler. That cortex's real executables run from here
//! was measured separately while the design was being written.
//!
//! `#[ignore]`, like `guest.rs` and for the same reasons: this needs libkrunfw installed, a
//! hypervisor the OS will let the process create, and on a cold cache a rootfs download.
//!
//! ```sh
//! cargo test -p cortex-uvm-console --test abin -- --ignored --test-threads=1
//! ```

use std::path::Path;
use std::process::Stdio;

use cortex::console::{Console, ExecResult};
use tokio::process::Command;

/// A directory of runnable scripts, each echoing something only it would.
fn dir_of(names: &[&str]) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    for name in names {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho ran-{name}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// A session whose cortex-provided executables are `builtin`, plus whatever `named` adds.
///
/// The override goes to the **server's** environment and not this process's: a console
/// server is configured by its environment, which is the one thing about it a client does not
/// say over the channel.
async fn session(builtin: &Path, named: &[&Path]) -> anyhow::Result<Console> {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
    server.stderr(Stdio::inherit());
    server.env("CORTEX_ABIN_DIR", builtin);
    let client = cortex::console::stdio::StdioClient::new(server)?;

    let mut builder = Console::builder().client(client);
    for dir in named {
        builder = builder.abin(dir);
    }
    builder.build().await
}

async fn out(console: &mut Console, script: &str) -> ExecResult {
    console
        .exec(["sh", "-c", script], None)
        .await
        .expect("running the command")
}

fn say(result: &ExecResult) -> String {
    String::from_utf8_lossy(&result.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn abin_is_a_read_only_disk_first_on_path() {
    let builtin = dir_of(&["mem", "index"]);
    let mut console = session(builtin.path(), &[]).await.expect("a session");

    assert_eq!(say(&out(&mut console, "ls /abin").await), "index\nmem");
    assert_eq!(say(&out(&mut console, "mem").await), "ran-mem");
    assert_eq!(say(&out(&mut console, "command -v mem").await), "/abin/mem");

    let path = say(&out(&mut console, r#"printf '%s' "$PATH""#).await);
    assert!(path.starts_with("/abin:"), "PATH is {path:?}");

    // The read-only claim, which is the whole reason this is a disk and not a share.
    let write = say(&out(&mut console, "touch /abin/x 2>&1; echo rc=$?").await);
    assert!(
        write.contains("rc=1"),
        "a write to /abin succeeded: {write:?}"
    );

    // And root in the guest cannot remount around it: the device itself refuses, which a
    // guest-side mount flag could not have done.
    let remount = say(&out(
        &mut console,
        "mount -o remount,rw /abin 2>&1; touch /abin/y 2>&1; echo rc=$?",
    )
    .await);
    assert!(
        remount.contains("rc=1"),
        "a write succeeded after remount: {remount:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn a_callers_executables_arrive_beside_cortexs() {
    let builtin = dir_of(&["mem"]);
    let caller = dir_of(&["mytool"]);
    let mut console = session(builtin.path(), &[caller.path()])
        .await
        .expect("a session");

    assert_eq!(say(&out(&mut console, "mytool").await), "ran-mytool");
    assert_eq!(say(&out(&mut console, "mem").await), "ran-mem");
}

/// A name cortex provides cannot be quietly replaced. The collision is found while the disk
/// is being assembled, which is on the way to a boot — so the session exists and simply has
/// no `/abin`, rather than running the wrong `mem`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn a_name_cortex_provides_is_not_shadowed() {
    let builtin = dir_of(&["mem"]);
    let caller = dir_of(&["mem"]);
    let mut console = session(builtin.path(), &[caller.path()])
        .await
        .expect("a session — the collision is found at boot, not at init");

    let found = say(&out(&mut console, "command -v mem; echo rc=$?").await);
    assert!(
        found.contains("rc=1"),
        "a shadowed mem was runnable: {found:?}"
    );
}

/// The control for the two above: without the override there is no `/abin` at all, so
/// finding one is finding something the test put there.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn without_executables_there_is_no_abin() {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
    server.stderr(Stdio::inherit());
    server.env_remove("CORTEX_ABIN_DIR");
    let client = cortex::console::stdio::StdioClient::new(server).unwrap();
    let mut console = Console::builder()
        .client(client)
        .build()
        .await
        .expect("a session still starts without one");

    assert_eq!(
        say(&out(&mut console, "test -d /abin; echo $?").await),
        "1",
        "/abin is there with nothing to put in it"
    );
    // And the session is otherwise exactly what it was.
    assert_eq!(
        say(&out(&mut console, "busybox true && echo ran").await),
        "ran"
    );
}

/// An image's own commands are still reachable — `/abin` goes in front of the `PATH`, not in
/// place of it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn an_image_keeps_its_own_commands() {
    let builtin = dir_of(&["mem"]);
    let mut console = session(builtin.path(), &[]).await.expect("a session");
    assert_eq!(
        say(&out(&mut console, "busybox true && echo ran").await),
        "ran"
    );
    assert!(
        say(&out(&mut console, "command -v sh").await).starts_with("/bin/"),
        "the image's own sh is gone"
    );
}
