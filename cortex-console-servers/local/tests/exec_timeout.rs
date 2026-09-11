//! `timeout_ms` is enforced: a command that outlives it is killed and answered with
//! `TIMED_OUT`, and the session goes on answering afterwards.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use cortex::{
    console::{Console, Error, stdio::StdioClient},
    fs::Mount,
};
use tokio::process::Command;

struct Mounted(PathBuf);

impl Mount for Mounted {
    fn mountpoint(&self) -> &Path {
        &self.0
    }
}

async fn console_over(root: &Path) -> anyhow::Result<Console> {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-local-console"));
    server.stderr(Stdio::inherit());
    let client = StdioClient::new(server)?;
    Console::builder()
        .client(client)
        .mount(Mounted(root.to_path_buf()))
        .build()
        .await
}

#[tokio::test]
async fn a_command_past_its_timeout_is_killed_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let mut console = console_over(dir.path()).await.unwrap();

    let started = Instant::now();
    let err = console
        .exec(["sh", "-c", "sleep 10"], Some(300))
        .await
        .expect_err("a 10s sleep under a 300ms timeout must be refused");
    assert_eq!(err.code(), Some(Error::TIMED_OUT), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the kill must not wait for the command: {:?}",
        started.elapsed()
    );

    // The session survives the kill.
    let ok = console
        .exec(["echo", "still-here"], Some(5_000))
        .await
        .unwrap();
    assert_eq!(ok.stdout, b"still-here\n");
}

/// The kill is a *group* kill: what the command started dies with it.
///
/// `sh -c 'sleep .. & sleep ..'` leaves a background child the shell never waits for, so
/// killing the direct child alone would leave it running with nobody to reap it. The
/// duration is a marker rather than a wait — nothing here sleeps for it; it is only what
/// makes `pgrep` match this test's processes and no one else's.
///
/// Fails if `process_group(0)` or the `killpg` goes away, and fails if the wait future is
/// handed to `timeout` by value again: the group leader would then be reaped before
/// `killpg` ran, and the background `sleep` would outlive the test.
#[tokio::test]
async fn the_kill_reaches_what_the_command_started() {
    const MARKER: &str = "sleep 37.31";

    // Nothing of this vintage may be running before the test, or the assertion below
    // would be reading someone else's process.
    assert!(
        !marker_is_running(MARKER).await,
        "a stray `{MARKER}` was already running — this test cannot tell it from its own"
    );

    let dir = tempfile::tempdir().unwrap();
    let mut console = console_over(dir.path()).await.unwrap();

    let err = console
        .exec(["sh", "-c", &format!("{MARKER} & {MARKER}")], Some(300))
        .await
        .expect_err("a 37s sleep under a 300ms timeout must be refused");
    assert_eq!(err.code(), Some(Error::TIMED_OUT), "{err:?}");

    // The signal and the processes' own exits are not synchronised with this end, so give
    // them a moment — but a moment far short of the sleep they would otherwise serve.
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        if !marker_is_running(MARKER).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("`{MARKER}` outlived the timeout kill — the group it was in was not signalled");
}

/// Whether any process whose command line contains `marker` is running.
async fn marker_is_running(marker: &str) -> bool {
    Command::new("pgrep")
        .arg("-f")
        .arg(marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("pgrep must be available")
        .success()
}

#[tokio::test]
async fn a_command_within_its_timeout_is_answered_normally() {
    let dir = tempfile::tempdir().unwrap();
    let mut console = console_over(dir.path()).await.unwrap();
    let ok = console
        .exec(["sh", "-c", "sleep 0.1; echo done"], Some(5_000))
        .await
        .unwrap();
    assert_eq!(ok.code, 0);
    assert_eq!(ok.stdout, b"done\n");
}

#[tokio::test]
async fn no_timeout_means_no_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut console = console_over(dir.path()).await.unwrap();
    let ok = console
        .exec(["sh", "-c", "sleep 0.2; echo ok"], None)
        .await
        .unwrap();
    assert_eq!(ok.stdout, b"ok\n");
}
