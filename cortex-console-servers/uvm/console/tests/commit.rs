//! A session writes, commits, and a second session boots what the first left.
//!
//! ```sh
//! cargo test -p cortex-uvm-console --test commit -- --ignored --test-threads=1
//! ```

use std::path::PathBuf;
use std::process::Stdio;

use cortex::console::{Console, ExecResp, ImageSource};
use tokio::process::Command;

/// Where this host keeps what sessions share.
fn home() -> PathBuf {
    std::env::var_os("CORTEX_UVM_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join(".cortex/uvm"))
}

/// An image this test made, removed whichever way the test ends.
///
/// A value and not a call: a call at the end of a test is one a failing assertion skips, and
/// what it would have skipped is an image left in the host's real store under a fixed id —
/// which the next run's `already_built` accepts and boots instead of making its own.
struct Kept(String);

impl Drop for Kept {
    fn drop(&mut self) {
        let stem = self.0.trim_start_matches("sha256:");
        for suffix in ["vmdk", "fsmeta.erofs", "manifest"] {
            let _ = std::fs::remove_file(home().join("built").join(format!("{stem}.{suffix}")));
        }
    }
}

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

async fn out(console: &mut Console, script: &str) -> ExecResp {
    console
        .exec(["sh", "-c", script], None)
        .await
        .expect("running the command")
}

fn say(result: &ExecResp) -> String {
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

/// An id the client chose, which is what the protocol says it is.
const ID: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

/// The whole of it: a session writes, commits, and a second session boots what the first left.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots two micro-VMs"]
async fn what_a_session_wrote_is_what_the_next_one_boots() {
    let _kept = Kept(ID.to_string());
    let mut first = session(None, true).await;

    assert_eq!(
        say(&out(&mut first, "echo kept > /marker; cat /marker").await),
        "kept"
    );
    assert_eq!(say(&out(&mut first, "rm -f /etc/motd; echo $?").await), "0");
    assert_eq!(
        say(&out(&mut first, "rm -rf /media && mkdir /media; echo $?").await),
        "0"
    );

    let image = first
        .commit(ID, vec!["BUILT_BY=cortex".into()], Some("/".into()))
        .await
        .expect("committing");
    assert_eq!(image.reference, format!("cortex.local/built@{ID}"));
    drop(first);

    let mut second = session(Some(image), false).await;

    // What the first session wrote, added and replaced.
    assert_eq!(say(&out(&mut second, "cat /marker").await), "kept");
    // What it deleted — the whiteout that travelled as a `0:0` character device.
    assert_eq!(
        say(&out(&mut second, "test -e /etc/motd; echo $?").await),
        "1",
        "the deletion did not survive the round trip"
    );
    // What it emptied — the one thing that had to be rewritten as a marker.
    assert_eq!(
        say(&out(&mut second, "ls -A /media | wc -l").await),
        "0",
        "the emptied directory came back full: the opaque marker was lost"
    );

    // The base is intact behind it and still runs.
    assert!(
        say(&out(&mut second, "cat /etc/alpine-release").await).starts_with("3."),
        "the base layer is gone"
    );
    assert_eq!(
        say(&out(&mut second, "busybox true && echo ran").await),
        "ran"
    );

    // And what the commit stated reaches a command.
    assert_eq!(
        say(&out(&mut second, r#"printf '%s' "$BUILT_BY""#).await),
        "cortex"
    );
}

/// A session that did not say it might commit cannot.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn a_session_that_did_not_say_so_cannot_commit() {
    let mut console = session(None, false).await;
    assert_eq!(say(&out(&mut console, "echo x > /marker").await), "");
    assert!(
        console.commit(ID, Vec::new(), None).await.is_err(),
        "a session that never said it might commit did"
    );
}

/// An image nobody made is said at `init`, before anything boots.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts a server"]
async fn an_image_that_was_never_built_is_refused_at_init() {
    let absent = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
    server.stderr(Stdio::inherit());
    let client = cortex::console::stdio::StdioClient::new(server).unwrap();
    let refused = Console::builder()
        .client(client)
        .image(ImageSource::new(format!("cortex.local/built@{absent}")))
        .build()
        .await;
    assert!(
        refused.is_err(),
        "a session started on an image nobody made"
    );
}

/// The machinery a session carries must not be in the image it commits.
///
/// **This cannot be asserted from inside a guest.** Every boot writes `/.cortex-guest` again
/// and a committable one recreates `/oldroot`, so a guest looking for them finds them whatever
/// the image holds. It is asserted against the layer instead.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn a_commit_keeps_only_what_the_session_wrote() {
    use cortex_uvm_console::built::BuiltStore;
    use cortex_uvm_console::layer::{LayerId, LayerStore, tree};

    let id = "sha256:3333333333333333333333333333333333333333333333333333333333333333";
    let _kept = Kept(id.to_string());
    let mut first = session(None, true).await;
    assert_eq!(
        say(&out(&mut first, "echo x > /only-this; echo $?").await),
        "0"
    );
    first
        .commit(id, Vec::new(), None)
        .await
        .expect("committing");
    drop(first);

    let built = BuiltStore::open(&home().join("built")).unwrap();
    let layers = LayerStore::open(&home().join("layers")).unwrap();
    let id = LayerId::parse(id).unwrap();

    let listed = built.layers(&id, &layers).expect("the image's layers");
    assert!(listed.len() >= 2, "a commit should have added a layer");

    let committed = listed
        .last()
        .unwrap()
        .tree(tree::Contents::Skip)
        .expect("the layer this session wrote");

    assert!(
        committed.get(b"only-this").is_some(),
        "the session\'s own file is not in the layer it committed"
    );
    for machinery in [
        &b".cortex-guest"[..],
        b"oldroot",
        b"abin",
        b".cortex-commit",
    ] {
        assert!(
            committed.get(machinery).is_none(),
            "{:?} was committed",
            String::from_utf8_lossy(machinery)
        );
    }
}
