//! A session writes, commits, and a second session boots what the first left.
//!
//! ```sh
//! cargo test -p cortex-uvm-console --test commit -- --ignored --test-threads=1
//! ```

use std::process::Stdio;

use cortex::console::{Console, ExecResult, ImageSource};
use tokio::process::Command;

async fn session(image: Option<ImageSource>, committable: bool) -> Console {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
    server.stderr(Stdio::inherit());
    let client = cortex::console::stdio::StdioClient::new(server).expect("a server");

    let mut builder = Console::builder().client(client);
    if let Some(image) = image {
        builder = builder.image(image);
    }
    if committable {
        builder = builder.committable();
    }
    builder.build().await.expect("a session")
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

/// What a commit needs arranged at boot, seen from inside the guest.
///
/// Not a commit yet — this is the ground it stands on. If the upperdir is not reachable there
/// is nothing for a commit to walk, and if the scratch is not writable there is nowhere to put
/// what it walked.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn a_committable_session_can_see_what_it_wrote() {
    let mut console = session(None, true).await;

    assert_eq!(say(&out(&mut console, "echo kept > /marker").await), "");

    // The overlay's upperdir, reachable because the old root was not detached.
    assert_eq!(
        say(&out(&mut console, "cat /oldroot/mnt/upper/upper/marker").await),
        "kept",
        "the session cannot see its own upperdir"
    );

    // A deletion is a character device there, which is what travels as a whiteout.
    assert_eq!(say(&out(&mut console, "rm -f /etc/motd").await), "");
    let kind = say(&out(
        &mut console,
        "stat -c '%F %t:%T' /oldroot/mnt/upper/upper/etc/motd",
    )
    .await);
    assert_eq!(
        kind, "character special file 0:0",
        "a deletion is not a 0:0 character device: {kind:?}"
    );

    // And somewhere to write the layer, which is the session's own and writable.
    assert_eq!(
        say(&out(
            &mut console,
            "echo x > /.cortex-commit/probe && cat /.cortex-commit/probe"
        )
        .await),
        "x",
        "the commit scratch is not writable"
    );
}

/// The control: an ordinary session has neither, and is exactly what it was.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn an_ordinary_session_has_neither() {
    let mut console = session(None, false).await;

    assert_eq!(
        say(&out(&mut console, "test -e /oldroot; echo $?").await),
        "1",
        "an ordinary session kept a root it will never look at"
    );
    assert_eq!(
        say(&out(&mut console, "test -e /.cortex-commit; echo $?").await),
        "1",
        "an ordinary session was given somewhere to write a layer it will never make"
    );
    assert_eq!(
        say(&out(&mut console, "busybox true && echo ran").await),
        "ran"
    );
}
