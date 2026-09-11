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
