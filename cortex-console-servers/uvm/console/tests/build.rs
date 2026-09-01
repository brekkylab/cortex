//! A build, booted: the image `cortex::rootfs` makes is one a session can run in.
//!
//! ```sh
//! cargo test -p cortex-uvm-console --test build -- --ignored --test-threads=1
//! ```

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use cortex::console::{Console, ConsoleBuilder, ExecResult, NetworkAccess};
use cortex::rootfs::Rootfs;
use tokio::process::Command;

/// A fresh server process, as a console builder.
///
/// A function rather than a value because `build` may open two consoles and each needs its
/// own process — which is the whole reason `build` takes a factory.
fn server() -> ConsoleBuilder {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
    server.stderr(Stdio::inherit());
    ConsoleBuilder::new()
        .client(cortex::console::stdio::StdioClient::new(server).expect("a server"))
}

async fn out(console: &mut Console, script: &str) -> String {
    let result = console
        .exec(["sh", "-c", script], None)
        .await
        .expect("running the command");
    String::from_utf8_lossy(&result.stdout).trim().to_string()
}

/// A build context with one file in it, and a fresh recipe every run.
///
/// The file's contents carry the salt: the build id covers what a `COPY` reads, so a
/// different byte is a different image — which is what keeps these tests from finding the
/// previous run's result in the cache and asserting nothing.
fn context(salt: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a context");
    std::fs::write(dir.path().join("marker.txt"), salt).expect("writing the context");
    dir
}

/// A unique-enough salt without a clock: the test's own name and this process's id.
fn salt(test: &str) -> String {
    format!("{test}-{}", std::process::id())
}

/// The whole of it. A base, a step of each kind, a deletion, and a directory emptied —
/// committed, and then a **new session** on the result where all of it holds.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn what_a_build_makes_is_what_the_next_session_boots() {
    let context = context(&salt("round-trip"));

    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();

    let built = Rootfs::from_image("alpine:3.20")
        .context(context.path().to_path_buf())
        // The one step here that needs it. A build gets the reach it asks for and no more,
        // and this is not in the build's id — how a build was made, not what it is.
        .network(NetworkAccess::public())
        .run("apk add --no-cache jq")
        .run("rm -f /etc/motd")
        .run("rm -rf /media && mkdir /media")
        .run("mkdir -p /srv")
        .copy("marker.txt", "/marker.txt")
        .env("BUILT_BY", "cortex")
        .workdir("/srv")
        .on_step(move |step, result: &ExecResult| {
            recorded
                .lock()
                .unwrap()
                .push((step.to_string(), result.code))
        })
        .build(server)
        .await
        .expect("building");

    assert_eq!(
        built.image().reference,
        format!("cortex.local/built@{}", built.id()),
        "a built image is not named the way the design says"
    );

    // Every step that reached the server, and none that did not: four `RUN`s, one `COPY`
    // and one `WORKDIR`. The `ENV` sends nothing.
    let steps = seen.lock().unwrap();
    assert_eq!(steps.len(), 6, "on_step saw {steps:?}");
    assert!(steps.iter().all(|(_, code)| *code == 0), "{steps:?}");
    drop(steps);

    let mut second = Console::builder()
        .client(
            cortex::console::stdio::StdioClient::new({
                let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-uvm-console"));
                server.stderr(Stdio::inherit());
                server
            })
            .expect("a server"),
        )
        .image(&built)
        .build()
        .await
        .expect("a session on the built image");

    // What a RUN installed.
    assert!(
        out(&mut second, "jq --version").await.starts_with("jq-"),
        "what the build installed is not there"
    );
    // What a RUN deleted — the whiteout.
    assert_eq!(out(&mut second, "test -e /etc/motd; echo $?").await, "1");
    // What a RUN emptied — the assertion that catches a lost `.wh..wh..opq`.
    assert_eq!(
        out(&mut second, "ls -A /media | wc -l").await,
        "0",
        "an emptied directory came back full"
    );
    // What a COPY brought in.
    assert_eq!(
        out(&mut second, "cat /marker.txt").await,
        salt("round-trip")
    );
    // What an ENV stated.
    assert_eq!(
        out(&mut second, r#"printf '%s' "$BUILT_BY""#).await,
        "cortex"
    );
    // What a WORKDIR stated.
    assert_eq!(out(&mut second, "pwd").await, "/srv");
    // And the base behind all of it.
    assert!(
        out(&mut second, "cat /etc/alpine-release")
            .await
            .starts_with("3."),
        "the base layer is gone"
    );
}

/// The same recipe twice: the second is answered from the cache, without booting.
///
/// Asserted by time, because there is nothing else to see from out here — a boot is seconds
/// and a probe is one process that starts and exits. The bound is generous on purpose: this
/// is meant to catch a cache that never hits, not to measure one that does.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn the_same_recipe_twice_boots_once() {
    let context = context(&salt("cache"));
    let recipe = || {
        Rootfs::from_image("alpine:3.20")
            .context(context.path().to_path_buf())
            .copy("marker.txt", "/marker.txt")
            .run("true")
    };

    let first = recipe().build(server).await.expect("the first build");

    let ran = std::time::Instant::now();
    let second = recipe().build(server).await.expect("the second build");
    let took = ran.elapsed();

    assert_eq!(first.id(), second.id(), "one recipe gave two ids");
    assert_eq!(first.image().reference, second.image().reference);
    assert!(
        took < std::time::Duration::from_secs(3),
        "the second build took {took:?}, which is long enough that it booted"
    );
}

/// A step that fails stops the build and commits nothing, so the next attempt starts over
/// rather than finding a half-built image in the cache.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots a micro-VM"]
async fn a_failed_build_leaves_nothing_behind() {
    let context = context(&salt("failure"));
    let recipe = || {
        Rootfs::from_image("alpine:3.20")
            .context(context.path().to_path_buf())
            .copy("marker.txt", "/marker.txt")
            .run("exit 7")
    };

    let refused = recipe().build(server).await.unwrap_err();
    let failed = refused
        .downcast_ref::<cortex::rootfs::StepFailed>()
        .unwrap_or_else(|| panic!("a step failure, and not {refused:#}"));
    assert_eq!(failed.result.code, 7);

    // The second attempt fails the same way. If the first had committed, this would find a
    // cached image and succeed.
    assert!(
        recipe().build(server).await.is_err(),
        "a failed build was cached"
    );
}
