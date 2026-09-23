//! `Backend::local`: a console whose server nobody installed.
//!
//! What these check is the part `Backend` owns — that the server this crate carries is written
//! out, run, told what the backend says, and cleaned up after — and not the server itself, whose
//! own suite is `cortex-local-console`'s.
//!
//!   cargo test -p cortex --features local --test backend_local
#![cfg(feature = "local")]

use std::path::Path;

use cortex::console::{Backend, Console};

/// Every `cortex-local-console-*` under `<home>/bin`.
fn servers(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(home.join("bin"))
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| name.starts_with("cortex-local-console-"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[tokio::test]
async fn a_local_console_runs_without_a_program_being_named() {
    let home = tempfile::tempdir().unwrap();
    let mut console = Console::builder()
        .backend(Backend::local().home(home.path()))
        .build()
        .await
        .unwrap();

    let result = console.exec(["sh", "-c", "echo hi"], None).await.unwrap();
    assert_eq!(result.stdout, b"hi\n");
    assert_eq!(
        servers(home.path()).len(),
        1,
        "written out once, under <home>/bin"
    );
    drop(console);

    // A second console finds it there rather than writing it again.
    let before = std::fs::metadata(home.path().join("bin").join(&servers(home.path())[0]))
        .unwrap()
        .modified()
        .unwrap();
    let mut console = Console::builder()
        .backend(Backend::local().home(home.path()))
        .build()
        .await
        .unwrap();
    console.exec(["true"], None).await.unwrap();
    let names = servers(home.path());
    assert_eq!(names.len(), 1);
    let after = std::fs::metadata(home.path().join("bin").join(&names[0]))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        before, after,
        "the second console did not rewrite the server"
    );
}

#[tokio::test]
async fn what_the_backend_says_reaches_the_server() {
    let home = tempfile::tempdir().unwrap();
    let mut console = Console::builder()
        .backend(
            Backend::local()
                .home(home.path())
                .env("CORTEX_BACKEND_TEST_MARK", "from-the-backend"),
        )
        .build()
        .await
        .unwrap();

    // A command inherits its server's environment, so what the server was told is what the
    // command sees.
    let result = console
        .exec(
            [
                "sh",
                "-c",
                "echo \"$CORTEX_BACKEND_TEST_MARK\" \"$CORTEX_HOME\"",
            ],
            None,
        )
        .await
        .unwrap();
    let expected = format!("from-the-backend {}\n", home.path().display());
    assert_eq!(String::from_utf8_lossy(&result.stdout), expected);
}

#[tokio::test]
async fn a_named_program_is_run_instead_and_nothing_is_written() {
    let home = tempfile::tempdir().unwrap();
    let Err(e) = Console::builder()
        .backend(
            Backend::local()
                .home(home.path())
                .program("/nonexistent/cortex-local-console"),
        )
        .build()
        .await
    else {
        panic!("a console over a program that does not exist should not build");
    };
    let message = format!("{e:#}");
    assert!(
        message.contains("/nonexistent/cortex-local-console"),
        "{message}"
    );
    assert!(
        servers(home.path()).is_empty(),
        "{:?}",
        servers(home.path())
    );
}

#[tokio::test]
async fn a_server_from_an_older_build_is_taken_away() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let stale = bin.join("cortex-local-console-0000000000000000");
    std::fs::write(&stale, b"an older server").unwrap();

    let mut console = Console::builder()
        .backend(Backend::local().home(home.path()))
        .build()
        .await
        .unwrap();
    console.exec(["true"], None).await.unwrap();

    assert!(!stale.exists(), "the older one is gone");
    assert_eq!(servers(home.path()).len(), 1);
}
