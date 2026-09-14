//! `/abin` in a running guest: are cortex's executables there, do they run, is the disk
//! really read-only, and is it first on `PATH`?
//!
//! A session neither names executables nor adds any — `/abin` is what cortex ships and
//! nothing else — so everything here is about delivering one fixed set.
//!
//! **Most of the executables here are shell scripts.** What those tests are about is the
//! delivery — the disk, the mount, the ordering, the refusals — and a script exercises all
//! of it without making the test depend on a musl cross-compiler or on anything published.
//! The last test in the file is the other half: no override, so the server resolves the
//! pointer and downloads the real release.
//!
//! `#[ignore]`, like `guest.rs` and for the same reasons: this needs libkrunfw installed, a
//! hypervisor the OS will let the process create, and on a cold cache a rootfs download.
//!
//! ```sh
//! cargo test -p cortex-uvm-console --test abin -- --ignored --test-threads=1
//! ```

use std::path::Path;
use std::process::Stdio;

use cortex::console::{Console, ExecResp};
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

/// A session whose cortex-provided executables are `builtin`.
///
/// The override goes to the **server's** environment and not this process's: a console
/// server is configured by its environment, and what a session runs is not something the
/// client says over the channel at all.
async fn session(builtin: &Path) -> anyhow::Result<Console> {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
    server.stderr(Stdio::inherit());
    server.env("CORTEX_ABIN_DIR", builtin);
    let client = cortex::console::stdio::StdioClient::new(server)?;

    Console::builder().client(client).build().await
}

async fn out(console: &mut Console, script: &str) -> ExecResp {
    console
        .exec(["sh", "-c", script], None)
        .await
        .expect("running the command")
}

fn say(result: &ExecResp) -> String {
    String::from_utf8_lossy(&result.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn abin_is_a_read_only_disk_first_on_path() {
    let builtin = dir_of(&["mem", "index"]);
    let mut console = session(builtin.path()).await.expect("a session");

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

/// The control: with nothing to put in it there is no `/abin` at all, so finding one
/// anywhere above is finding something the test put there.
///
/// "Nothing" takes three denials now that releases exist. No override, a base URL with no
/// pointer under it, and a home of its own so that a release this host downloaded earlier is
/// not sitting in the cache — miss any one and the server finds a perfectly good `/abin`.
///
/// Which makes this the test for the rule that a session is never refused over `/abin`: every
/// way of getting one has failed here, and a session still starts.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn without_executables_there_is_no_abin() {
    let nowhere = tempfile::tempdir().expect("an empty directory");
    let home = tempfile::tempdir().expect("a home");

    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
    server.stderr(Stdio::inherit());
    server.env_remove("CORTEX_ABIN_DIR");
    server.env_remove("CORTEX_ABIN_VERSION");
    server.env("CORTEX_UVM_HOME", home.path());
    server.env(
        "CORTEX_ABIN_BASE_URL",
        format!("file://{}", nowhere.path().display()),
    );
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
    let mut console = session(builtin.path()).await.expect("a session");
    assert_eq!(
        say(&out(&mut console, "busybox true && echo ran").await),
        "ran"
    );
    assert!(
        say(&out(&mut console, "command -v sh").await).starts_with("/bin/"),
        "the image's own sh is gone"
    );
}

/// The published release, end to end: no `CORTEX_ABIN_DIR`, so the server resolves the
/// pointer, downloads the tarball, and the executables in it run in the guest.
///
/// Reaches the network on purpose — it is the only test that proves the bucket, the key
/// layout and the client agree. A home of its own, so it proves a cold download rather than
/// finding whatever this host already had.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM and downloads the published /abin"]
async fn the_published_release_runs_in_a_guest() {
    let home = tempfile::tempdir().expect("a home");

    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
    server.stderr(Stdio::inherit());
    server.env("CORTEX_UVM_HOME", home.path());
    // All three, not just the first. This is the one test whose subject is the *published*
    // release; a developer with either of the others exported would silently be testing
    // their staging bucket or a pinned old build, and it would still pass.
    server.env_remove("CORTEX_ABIN_DIR");
    server.env_remove("CORTEX_ABIN_BASE_URL");
    server.env_remove("CORTEX_ABIN_VERSION");

    let mut console = Console::builder()
        .client(cortex::console::stdio::StdioClient::new(server).expect("a server"))
        .build()
        .await
        .expect("a session");

    for (script, expected) in [
        ("command -v mem", "/abin/mem"),
        ("command -v index", "/abin/index"),
    ] {
        let out = console
            .exec(["sh", "-c", script], None)
            .await
            .expect("running it");
        assert_eq!(
            out.code,
            0,
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected);
    }

    // And they are executables that work, not files that happen to be in the right place.
    for name in ["mem", "index"] {
        let out = console
            .exec(["sh", "-c", &format!("{name} --help")], None)
            .await
            .expect("running it");
        assert_eq!(
            out.code,
            0,
            "{name} --help: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.stdout.is_empty(), "{name} --help said nothing");
    }
}
