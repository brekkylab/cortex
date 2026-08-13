//! A session's namespace, over the real binary.
//!
//! The namespace is the client's to declare and the server's to realize, so nothing
//! smaller than two processes tells you whether the two agree. These boot the real
//! binary and ask it for things a namespace changes the answer to.
//!
//! # The ones that mount take a lock first
//!
//! In a build with a mount binding, four of these put a real FUSE mount on this machine.
//! Tearing several down at once is the one thing known to wedge it: five concurrent
//! `FuseTMount` teardowns left one in uninterruptible sleep for 145 seconds, out of reach
//! of `kill`, until it was unmounted from outside by hand.
//!
//! [`cortex/tests/host_mount.rs`] answers that with `#[ignore]`, so a plain `cargo test`
//! never mounts anything. That does not fit here: these four are the evidence that a
//! declared namespace reaches a command at all, and evidence nobody runs protects nothing.
//! So they run by default and serialize themselves through [`MOUNTS`] instead — no flag to
//! remember, and no way for a forgotten one to cost somebody an afternoon.
//!
//! `--test-threads=1` is still the belt to the lock's braces, and still what to use when
//! something here has gone wrong.

// Holding it across an `await` is the whole point — a mount outlives the calls made
// against it — and [`MOUNTS`] says why a `std` lock is the right one here anyway. Allowed
// once, at the file it is true of, rather than four times at the tests that take it.
#![allow(clippy::await_holding_lock)]

use std::process::Stdio;

use cortex::console::Console;
use cortex::console::stdio::StdioClient;
use cortex::volume::{NotionConfig, VolumeSpec, WorkspaceSpec};
use tokio::process::Command;

/// A console over the real server binary, with `stderr` left where a person can see it.
fn server() -> StdioClient {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-local-console"));
    server.stderr(Stdio::inherit());
    StdioClient::new(server).expect("starting the console server")
}

/// The same, but with the server's `$TMPDIR` pointed somewhere only one test can see.
///
/// For the one test that reads the scratch directories back: `$TMPDIR` is shared with
/// every other server this suite starts, and a server is **killed** when its console
/// drops rather than asked to quit, so its `Drop` never runs and its root outlives it.
/// Those roots are indistinguishable from a leak unless each test gets its own directory.
fn server_under(tmp: &std::path::Path) -> StdioClient {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-local-console"));
    server.stderr(Stdio::inherit()).env("TMPDIR", tmp);
    StdioClient::new(server).expect("starting the console server")
}

/// Held for as long as a test has a mount on this machine, so only one ever does.
///
/// A `std::sync::Mutex` and not a `tokio` one on purpose: `#[tokio::test]` gives each test
/// its own current-thread runtime, so a guard taken at the top of a test body never moves
/// between threads, and a blocking lock here blocks a whole test rather than a task that
/// something else is waiting on.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
static MOUNTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`MOUNTS`], surviving a poisoning.
///
/// A test that panicked while mounted poisoned it, and that is a failure to report on its
/// own — not a reason to fail every test after it with a different message.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
fn one_mount_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    MOUNTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A namespace this build has no provider for.
fn unrealizable() -> WorkspaceSpec {
    WorkspaceSpec::default().mount(
        "docs",
        VolumeSpec::Notion(NotionConfig {
            api_key: "a".into(),
        }),
    )
}

/// A volume kind this build has no provider for is refused when a session boots — which
/// is the first call that needs one, not `init`.
#[tokio::test]
async fn a_volume_kind_this_build_cannot_realize_is_refused_when_something_needs_a_session() {
    let mut console = Console::builder()
        .client(server())
        .volumes(unrealizable())
        .build()
        .await
        .expect("init is answered — it realizes nothing");

    // The failure lands here, on the call that needed a session.
    let err = console
        .exec(["true"], None)
        .await
        .expect_err("no session can be built from a kind this build lacks");
    assert_eq!(err.code(), Some(cortex::console::Error::UNSUPPORTED_VOLUME));
}

/// A failed realization does not wedge the session: a later call is answered, and answered
/// the same way.
///
/// What it catches is a session that goes *quiet* or *different* after a failed boot — not
/// literally that `from_spec` runs twice, which the wire cannot distinguish from a replayed
/// answer. `self.booted` only ever moves `None → Some` on success, so there is nothing to
/// cache; this guards the shape a client relies on rather than the implementation.
#[tokio::test]
async fn a_failed_realization_retries_rather_than_wedging() {
    let mut console = Console::builder()
        .client(server())
        .volumes(unrealizable())
        .build()
        .await
        .unwrap();

    for attempt in 1..=2 {
        let err = console
            .exec(["true"], None)
            .await
            .expect_err("cannot realize");
        assert_eq!(
            err.code(),
            Some(cortex::console::Error::UNSUPPORTED_VOLUME),
            "attempt {attempt}: the same answer, not a cached or degraded one"
        );
    }
}

/// And a session with no volumes still works exactly as it did.
#[tokio::test]
async fn a_session_with_no_volumes_runs_commands_as_before() {
    let mut console = Console::builder().client(server()).build().await.unwrap();

    let out = console.exec(["sh", "-c", "echo hi"], None).await.unwrap();
    assert_eq!(out.stdout, b"hi\n");
}

/// Scratch roots under `tmp` whose owning process is gone — a leak, by the same test the
/// sweeper applies.
///
/// The `cortex-console-<pid>` naming is not incidental: it is what `sweep_stale` reads to
/// tell an abandoned root from a live one, so a test reads it the same way and learns the
/// pid of a server it only ever spoke to over a pipe.
#[cfg(not(any(feature = "fuse", feature = "fuse-t")))]
fn abandoned_roots(tmp: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(tmp)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let pid: i32 = name.strip_prefix("cortex-console-")?.parse().ok()?;
            // SAFETY: signal 0 performs only the permission/existence check.
            // Parenthesised because an `unsafe` block in statement position parses as a
            // statement, and the comparison would have nothing to bind to.
            let gone = (unsafe { libc::kill(pid, 0) }) == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            gone.then(|| e.path())
        })
        .collect()
}

/// A build with no mount binding says so, and says it about the binding rather than the
/// volume — the kind was fine.
#[cfg(not(any(feature = "fuse", feature = "fuse-t")))]
#[tokio::test]
async fn a_build_with_no_mount_binding_refuses_a_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let spec = WorkspaceSpec::default().mount(
        "work",
        VolumeSpec::Local {
            host: dir.path().to_path_buf(),
        },
    );

    // Its own `$TMPDIR`, so the roots read back below are this server's and nobody else's.
    let tmp = tempfile::tempdir().unwrap();

    let mut console = Console::builder()
        .client(server_under(tmp.path()))
        .volumes(spec)
        .build()
        .await
        .unwrap();

    let err = console
        .exec(["true"], None)
        .await
        .expect_err("nothing can mount it");
    assert_eq!(err.code(), Some(cortex::console::Error::MOUNT_FAILED));

    // This is the ordering the realization test could not reach: the scratch directory
    // *was* created and a later step of the same boot failed, so the rollback is
    // observable.
    //
    // The observable is *abandonment*, not existence. `mnt/` exists for every session —
    // it is created unconditionally — so "some root has an `mnt/`" is satisfied by any
    // live session in this binary, and none of these tests is pinned to one thread. A root
    // whose pid is gone is the thing that leaked, and it is exactly the test `sweep_stale`
    // applies for the same reason.
    //
    // The console has to be dropped first so this server's own pid is gone either way;
    // what is being asserted is that it took its directory with it.
    drop(console);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert!(
        abandoned_roots(tmp.path()).is_empty(),
        "a boot that failed past the scratch left it behind: {:?}",
        abandoned_roots(tmp.path())
    );
}

/// Mountpoints the OS reports under `root`, which must already be canonical.
///
/// The mount table rather than the filesystem, and no row is resolved here: `mount(8)`
/// lists the whole machine, and walking into somebody else's wedged mount to canonicalize
/// it is the one thing a test about mounts must never do.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
fn mounts_under(root: &std::path::Path) -> Vec<String> {
    let out = std::process::Command::new("mount")
        .output()
        .expect("mount(8) runs");
    let root = root.to_string_lossy().into_owned();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        // `fuse-t:/tag on /path (nfs, …)` — the mountpoint is between " on " and " (".
        .filter_map(|l| l.split(" on ").nth(1))
        .filter_map(|rest| rest.split(" (").next())
        .filter(|m| m.starts_with(&root))
        .map(str::to_owned)
        .collect()
}

/// A declared namespace is mounted — by the call that needs a session, not by `init` — and
/// it is mounted inside that session's own scratch.
///
/// This is what task 3 alone can show. Whether a *command* can then reach it by a relative
/// name is a different claim, because it needs the command's working directory set, and
/// nothing here does that yet.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
#[tokio::test]
async fn a_declared_namespace_is_mounted_by_the_call_that_needs_a_session() {
    let _mounted = one_mount_at_a_time();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();

    let spec = WorkspaceSpec::default().mount(
        "work",
        VolumeSpec::Local {
            host: dir.path().to_path_buf(),
        },
    );

    // Its own `$TMPDIR`, so what is mounted under it is this server's and nobody else's.
    // Canonicalized once, here, while it is an ordinary directory — the mount table
    // reports `/private/var/…` where `$TMPDIR` says `/var/…`.
    let tmp = tempfile::tempdir().unwrap();
    let under = tmp.path().canonicalize().unwrap();

    let mut console = Console::builder()
        .client(server_under(tmp.path()))
        .volumes(spec)
        .build()
        .await
        .unwrap();

    assert!(
        mounts_under(&under).is_empty(),
        "`init` boots nothing, so it mounts nothing"
    );

    console.exec(["true"], None).await.unwrap();

    let mounted = mounts_under(&under);
    assert_eq!(mounted.len(), 1, "one namespace, one mount: {mounted:?}");
    assert!(
        mounted[0].ends_with("/mnt"),
        "mounted on the scratch's own mount point: {mounted:?}"
    );

    // And released when the session ends. The server exits on EOF and unmounts on the way
    // out, so this is polled rather than asserted once — what matters is that it happens,
    // not that it has happened by the time the next line runs.
    drop(console);
    for _ in 0..100 {
        if mounts_under(&under).is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the mount outlived the session: {:?}", mounts_under(&under));
}

/// A command runs *in* the namespace, so a relative path is one of its names.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
#[tokio::test]
async fn a_command_runs_with_the_namespace_as_its_working_directory() {
    let _mounted = one_mount_at_a_time();

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/deep.txt"), b"found").unwrap();

    let spec = WorkspaceSpec::default().mount(
        "work",
        VolumeSpec::Local {
            host: dir.path().to_path_buf(),
        },
    );

    let mut console = Console::builder()
        .client(server())
        .volumes(spec)
        .build()
        .await
        .unwrap();

    // `cd` proves the cwd is the mount and not whatever this process inherited.
    let out = console
        .exec(["sh", "-c", "cd work/sub && cat deep.txt"], None)
        .await
        .unwrap();
    assert_eq!(
        out.code,
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"found");
}

/// The invariant the module doc states: the two halves of the protocol name the same file.
///
/// Without the `read`/`write` half of this, `exec` finds the file and `read` answers
/// `NOT_FOUND` for the identical path — resolved against wherever the binary was launched.
/// Nothing else here would catch that.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
#[tokio::test]
async fn read_names_the_same_file_a_command_would() {
    let _mounted = one_mount_at_a_time();

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();

    let spec = WorkspaceSpec::default().mount(
        "work",
        VolumeSpec::Local {
            host: dir.path().to_path_buf(),
        },
    );

    let mut console = Console::builder()
        .client(server())
        .volumes(spec)
        .build()
        .await
        .unwrap();

    let by_command = console.exec(["cat", "work/hello.txt"], None).await.unwrap();
    assert_eq!(by_command.stdout, b"hi");

    let by_read = console
        .read("work/hello.txt", None, None)
        .await
        .expect("the same name the command just opened");
    assert_eq!(by_read.data, b"hi");
}

/// The whole point, end to end: a delegated executable resolves the same file the command
/// would have.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
#[tokio::test]
async fn a_delegated_call_resolves_the_file_the_command_would_have() {
    let _mounted = one_mount_at_a_time();

    // `cortex::BoxFuture`, not `futures_core` — this crate does not depend on it, and
    // `tests/delegate.rs` reaches it the same way.
    use cortex::BoxFuture;
    use cortex::executable::{ExecCall, ExecResult as ExecOutput, Executable, ExecutableSet};
    use cortex::volume::Workspace;

    /// Reports what it resolved, against the same spec the server was given.
    struct Where;
    impl Executable for Where {
        fn exec<'a>(
            &'a self,
            call: &'a ExecCall,
            _workspace: &'a Workspace,
        ) -> BoxFuture<'a, ExecOutput> {
            Box::pin(async move {
                match call.resolve(&call.args[0]) {
                    Ok(p) => ExecOutput::ok(p.to_string_lossy().into_owned()),
                    Err(e) => ExecOutput::failed(1, format!("{e}")),
                }
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    let spec = WorkspaceSpec::default().mount(
        "work",
        VolumeSpec::Local {
            host: dir.path().to_path_buf(),
        },
    );

    let mut console = Console::builder()
        .client(server())
        .volumes(spec)
        .executables(ExecutableSet::new().register("where", Where))
        .build()
        .await
        .unwrap();

    let out = console
        .exec(["sh", "-c", "cd work/sub && where report.md"], None)
        .await
        .unwrap();
    assert_eq!(
        out.code,
        0,
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "work/sub/report.md",
        "the executable resolved against the same namespace, from where the command stood"
    );
}

/// **One name, one file, three ways.** A command, a `read`, and a delegated executable each
/// name `work/report.md`, and all three come back with the same bytes.
///
/// The test above shows the executable *resolving* the caller's name. This one shows it
/// **opening** the result — against a `Workspace` this process built from the same spec the
/// server was sent, so nothing here reads through the server's tree. Agreement on arithmetic
/// and agreement on a file are two claims, and only the second is the point.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
#[tokio::test]
async fn one_name_is_one_file_to_a_command_a_read_and_a_delegated_call() {
    use cortex::BoxFuture;
    use cortex::executable::{ExecCall, ExecResult, Executable, ExecutableSet};
    use cortex::volume::{Mountable as _, OpenOptions, Workspace};

    /// Resolves the caller's argument and **opens** it, through the tree it was handed —
    /// the console's, which this process built from the spec the server was sent.
    struct Reader;

    impl Executable for Reader {
        fn exec<'a>(
            &'a self,
            call: &'a ExecCall,
            workspace: &'a Workspace,
        ) -> BoxFuture<'a, ExecResult> {
            Box::pin(async move {
                let path = match call.resolve(&call.args[0]) {
                    Ok(path) => path,
                    Err(e) => return ExecResult::failed(1, format!("resolve: {e}")),
                };
                let (handle, stat) = match workspace.open(&path, OpenOptions::read_only()).await {
                    Ok(open) => open,
                    Err(e) => return ExecResult::failed(1, format!("open {path:?}: {e}")),
                };
                let mut buf = vec![0u8; stat.size as usize];
                if let Err(e) = handle.read_exact_at(&mut buf, 0).await {
                    return ExecResult::failed(1, format!("read: {e}"));
                }
                // Both halves, so a right answer can be told from a right-looking one.
                ExecResult::ok(format!(
                    "{}|{}",
                    path.display(),
                    String::from_utf8_lossy(&buf)
                ))
            })
        }
    }

    let _mounted = one_mount_at_a_time();

    let host = tempfile::tempdir().unwrap();
    std::fs::write(host.path().join("report.md"), b"the same bytes\n").unwrap();

    let spec = WorkspaceSpec::default().mount(
        "work",
        VolumeSpec::Local {
            host: host.path().to_path_buf(),
        },
    );
    // Two `from_spec` calls, one description: the server's happens when it boots.
    let mine = Workspace::from_spec(&spec).expect("the client realizes it too");

    let mut console = Console::builder()
        .client(server())
        .volumes(spec)
        .workspace(mine)
        .executables(ExecutableSet::new().register("readit", Reader))
        .build()
        .await
        .unwrap();

    let by_command = console
        .exec(["cat", "work/report.md"], None)
        .await
        .expect("a command runs in the mounted namespace");
    let by_read = console
        .read("work/report.md", None, None)
        .await
        .expect("a file call resolves in the same one");
    // Relative, from inside `work`, so this only works if `cwd` arrived.
    let by_delegate = console
        .exec(["sh", "-c", "cd work && readit report.md"], None)
        .await
        .expect("the delegated call is served");

    let reported = String::from_utf8_lossy(&by_delegate.stdout).into_owned();
    let (resolved, content) = reported
        .split_once('|')
        .unwrap_or_else(|| panic!("the executable failed: {reported}"));

    assert_eq!(by_command.stdout, b"the same bytes\n", "the command");
    assert_eq!(by_read.data, b"the same bytes\n", "the protocol's own read");
    assert_eq!(resolved, "work/report.md", "what the executable resolved to");
    assert_eq!(content, "the same bytes\n", "what the executable read");
}
