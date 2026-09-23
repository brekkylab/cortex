//! A console that names no backend runs on the local one.
//!
//! A file of its own, with one test in it, because the only way to point a console that was told
//! nothing at a directory of the test's is `CORTEX_HOME` — and setting a variable is only sound
//! while no other thread is reading the environment.
#![cfg(feature = "local")]

use cortex::console::Console;

#[tokio::test]
async fn a_console_told_nothing_runs_on_this_host() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary, on a current-thread runtime that has started no
    // thread of its own yet.
    unsafe { std::env::set_var("CORTEX_HOME", home.path()) };

    let mut console = Console::builder().build().await.unwrap();
    let result = console.exec(["sh", "-c", "uname -s"], None).await.unwrap();
    assert_eq!(
        String::from_utf8_lossy(&result.stdout).trim(),
        std::process::Command::new("uname")
            .arg("-s")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap(),
        "the command ran on this host"
    );

    let written: Vec<_> = std::fs::read_dir(home.path().join("bin"))
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    assert!(
        written
            .iter()
            .any(|name| name.starts_with("cortex-local-console-")),
        "the local server was written under $CORTEX_HOME/bin: {written:?}"
    );
}
