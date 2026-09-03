//! End-to-end over the real binary: the tree a session works in, and where it stands in it.
//!
//! Both are things only the far end can settle — it is the process that answers `init`,
//! spawns the commands and holds the directory between them — so the only way to see what
//! this backend does with them is to drive it over the channel and ask.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use cortex::{
    console::{Console, Error, ExecResp, stdio::StdioClient},
    fs::Mount,
};
use tempfile::TempDir;
use tokio::process::Command;

/// A directory standing in for a mounted tree.
///
/// Not a mount this test made: putting one up needs a binding, a libfuse provider and a
/// kernel, which is what `cortex/tests/host_mount.rs` is for. What is under test here is
/// what the *server* does with the path it is told, and a plain directory answers one the
/// same way a mount point does.
struct Mounted(PathBuf);

impl Mount for Mounted {
    fn mountpoint(&self) -> &Path {
        &self.0
    }
}

/// A console over the real binary, with a tree under it.
struct Fixture {
    console: Console,
    /// Held for the test's lifetime and read by nobody: dropping it removes the directory.
    _dir: TempDir,
    /// The mount point, as it was handed over and **not** canonicalized.
    ///
    /// Which is the ordinary case rather than an awkward one: a temp directory on macOS is
    /// reached through a symlink (`/var` → `/private/var`), and so is `/tmp`. Every path
    /// the server reports has to come back in this spelling, because it is the only one the
    /// client was told.
    root: PathBuf,
}

impl Fixture {
    async fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("a temp directory");
        let root = dir.path().to_path_buf();
        let console = console_over(&root).await.expect("building the console");
        Fixture {
            console,
            _dir: dir,
            root,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    async fn output(&mut self, script: &str) -> ExecResp {
        self.console
            .exec(["sh", "-c", script], None)
            .await
            .expect("running the command")
    }

    /// `cd`, which this backend answers itself rather than spawning.
    async fn cd(&mut self, args: &[&str]) -> ExecResp {
        let mut cmd = vec!["cd"];
        cmd.extend_from_slice(args);
        self.console.exec(cmd, None).await.expect("running cd")
    }

    /// Where the session stands, asked for the way a person at a terminal asks.
    ///
    /// Nothing reports it: a result describes the command, so this is the only reading of
    /// the session's own directory there is after `init` answered where it started.
    async fn pwd(&mut self) -> PathBuf {
        let out = self.output("pwd").await;
        PathBuf::from(
            String::from_utf8(out.stdout)
                .expect("a path that is text")
                .trim_end(),
        )
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

/// `init` names the tree by its mount point and the server answers where it put it — the
/// same directory, since a `file://` workfs is one this host already has and there is
/// nothing to interpose.
///
/// And the session stands in it: a command runs there without anything on its own frame
/// saying so.
#[tokio::test]
async fn a_session_stands_in_the_tree_it_was_given() {
    let mut fx = Fixture::new().await;

    assert_eq!(fx.console.workfs_path(), Some(fx.root.as_path()));
    assert_eq!(
        fx.output("pwd").await.stdout,
        format!("{}\n", fx.root.display()).into_bytes()
    );

    // A relative path in a command is therefore the tree's, which is the whole point of
    // standing anywhere.
    fx.output("echo written > note.txt").await;
    assert_eq!(
        std::fs::read_to_string(fx.path("note.txt")).unwrap(),
        "written\n"
    );
}

/// `cd` moves the session, and the command after it runs there.
///
/// It is not a program — no `cd` exists on `PATH` — so this is the server answering as the
/// shell a person would be talking to, which is the only place a console session's
/// directory could live.
///
/// **`pwd` is how the move is observed**, here and everywhere below. A `cd` answers with a
/// code and nothing else, exactly as a terminal does, so where the session ended up is a
/// question a client asks rather than something every result repeats.
#[tokio::test]
async fn cd_moves_the_session_and_the_next_command_runs_there() {
    let mut fx = Fixture::new().await;
    std::fs::create_dir(fx.path("work")).unwrap();

    let moved = fx.cd(&["work"]).await;
    assert_eq!(moved.code, 0);
    assert!(moved.stdout.is_empty(), "a cd that worked says nothing");
    assert_eq!(fx.pwd().await, fx.path("work"));

    // Relative from where it now stands, and `..` gets back — resolved by the server, so
    // where it lands is a path with nothing left to resolve.
    assert_eq!(fx.cd(&[".."]).await.code, 0);
    assert_eq!(fx.pwd().await, fx.root);

    // And `cd` with nothing is the tree, the way a shell's is `$HOME`.
    fx.cd(&["work"]).await;
    fx.cd(&[]).await;
    assert_eq!(fx.pwd().await, fx.root);
}

/// An ordinary command leaves the session where it was, however it ends: a `cd` inside one
/// moves that command's own process, which is what a subshell does in a terminal too.
#[tokio::test]
async fn a_command_that_cds_inside_itself_moves_nothing() {
    let mut fx = Fixture::new().await;
    std::fs::create_dir(fx.path("work")).unwrap();

    let out = fx.output("cd work && pwd").await;
    assert_eq!(
        out.stdout,
        format!("{}\n", fx.path("work").display()).into_bytes(),
        "the command itself did move"
    );

    assert_eq!(fx.pwd().await, fx.root, "and the session did not");
}

/// A `cd` that cannot happen is the builtin failing, not the call: a code and a line on
/// stderr, the session where it was, and the channel still good.
#[tokio::test]
async fn a_cd_that_cannot_happen_leaves_the_session_where_it_was() {
    let mut fx = Fixture::new().await;

    let refused = fx.cd(&["nowhere"]).await;
    assert_eq!(refused.code, 1);
    assert!(
        String::from_utf8_lossy(&refused.stderr).starts_with("cd: "),
        "{:?}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(fx.pwd().await, fx.root, "nothing moved");
}

/// The file plane stands in the same place: a relative path is resolved where the session
/// is, so a `read` names the file a command would open by the same name — before and after
/// a `cd`.
#[tokio::test]
async fn a_file_call_lands_where_the_session_stands() {
    let mut fx = Fixture::new().await;
    std::fs::create_dir(fx.path("work")).unwrap();

    fx.console
        .write("note.txt", b"at the root".as_slice(), None)
        .await
        .expect("writing");
    assert_eq!(
        std::fs::read_to_string(fx.path("note.txt")).unwrap(),
        "at the root"
    );

    fx.cd(&["work"]).await;
    fx.console
        .write("note.txt", b"in the subdirectory".as_slice(), None)
        .await
        .expect("writing");
    assert_eq!(
        std::fs::read_to_string(fx.path("work/note.txt")).unwrap(),
        "in the subdirectory"
    );

    // Which is the same file the command standing there opens.
    assert_eq!(
        fx.output("cat note.txt").await.stdout,
        b"in the subdirectory"
    );

    // An absolute path is already an answer and ignores all of it.
    let read = fx
        .console
        .read(fx.path("note.txt").to_str().unwrap(), None, None)
        .await
        .expect("reading");
    assert_eq!(read.data, b"at the root");
}

/// A tree that is not there is the *environment* being wrong for a session that is
/// described correctly, so it is `MOUNT_FAILED` — and it arrives on the first call that
/// needed a boot rather than at `init`, because `init` says where the tree will be and a
/// boot is what has to find it.
#[tokio::test]
async fn a_tree_that_is_not_there_fails_the_boot_and_not_the_init() {
    let dir = tempfile::tempdir().expect("a temp directory");
    let gone = dir.path().join("not-created");

    // The session is taken: the URL is well formed and this build realizes `file://`.
    let mut console = console_over(&gone).await.expect("building the console");
    assert_eq!(console.workfs_path(), Some(gone.as_path()));

    let refused = console
        .exec(["true"], None)
        .await
        .expect_err("running in a directory that is not there");
    assert_eq!(refused.code(), Some(Error::MOUNT_FAILED));

    // And a `read`, which needs a session for the same reason, hears the same thing.
    let refused = console
        .read("anything", None, None)
        .await
        .expect_err("reading in a directory that is not there");
    assert_eq!(refused.code(), Some(Error::MOUNT_FAILED));
}

/// A `stop` hands back what booting took and not where the session stands: the path a
/// client built before going idle is still the path it is standing on afterwards.
#[tokio::test]
async fn stopping_does_not_move_the_session() {
    let mut fx = Fixture::new().await;
    std::fs::create_dir(fx.path("work")).unwrap();

    fx.cd(&["work"]).await;
    fx.console.stop().await.expect("stopping");

    assert_eq!(fx.pwd().await, fx.path("work"));
}

/// **Where the session stands is spelled the way the path `init` answered with is.**
///
/// A mount point reached through a symlink is the ordinary case, and the two spellings of
/// it are not interchangeable to a client: it was told one, and a directory that turned up
/// in the other would be a path it has no way to relate to anything. `getcwd(2)` answers
/// the physical one, so this is something the server has to put right rather than something
/// that holds by itself — it moves the session logically and hands the command a `PWD` to
/// match.
#[tokio::test]
async fn what_the_server_reports_is_spelled_the_way_init_answered() {
    let mut fx = Fixture::new().await;
    std::fs::create_dir(fx.path("work")).unwrap();

    let physical = fx.root.canonicalize().unwrap();
    assert_ne!(
        physical, fx.root,
        "this test needs a mount point reached through a symlink"
    );

    // `init`: what was asked for.
    assert_eq!(fx.console.workfs_path(), Some(fx.root.as_path()));

    // `cd`: normalized on paper, not canonicalized — so `pwd` in the command that follows
    // answers under the path the client was told, and not under `/private/…`.
    fx.cd(&["work"]).await;
    assert_eq!(fx.pwd().await, fx.path("work"));

    // And a `read` names a file under it, which is the whole of what the spelling is for:
    // the path the client builds is the one a command standing there would have opened.
    std::fs::write(fx.path("work/note.txt"), b"resolved\n").unwrap();
    let read = fx
        .console
        .read(fx.path("work/note.txt").to_str().unwrap(), None, None)
        .await
        .expect("reading the file the client named");
    assert_eq!(read.data, b"resolved\n");
}

/// A base image is refused here, and named.
///
/// The one thing this backend cannot be asked for. A command here is a process on this host,
/// running against the filesystem this server can already see: there is no root to swap and no
/// overlay to put one under, so an image is not a narrower session this could give but a
/// different backend entirely. Refusing says so while the client can still pick one.
#[tokio::test]
async fn a_host_local_session_has_no_base_to_swap() {
    // Asking for nothing is not asking for less: the session every client had before there was
    // a base to name.
    let console = built_on(None).await.expect("a session that named nothing");
    assert_eq!(
        console.image(),
        None,
        "this backend named a base it has not got"
    );
    drop(console);

    let err = match built_on(Some("python:3.13-slim".into())).await {
        Err(e) => e,
        Ok(_) => panic!("a session was opened on an image this backend cannot provide"),
    };
    let failure = err
        .downcast_ref::<cortex::console::Failure>()
        .expect("a protocol failure");
    assert_eq!(
        failure.code(),
        Some(Error::UNSUPPORTED_IMAGE),
        "refused as {failure:?}, which is a different problem"
    );
}

/// A console over the real binary with no tree, naming a base or naming none.
async fn built_on(image: Option<cortex::console::ImageSource>) -> anyhow::Result<Console> {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-local-console"));
    server.stderr(Stdio::inherit());

    let mut builder = Console::builder().client(StdioClient::new(server)?);
    if let Some(image) = image {
        builder = builder.image(image);
    }
    builder.build().await
}

/// A host-local session reaches whatever this host does, and says so.
///
/// Which is the one thing this backend can answer about a network. There is no device to leave
/// off and no policy to put over one — a command here is a process on this host — so the honest
/// answers are `full` and a refusal, and the refusal is what a client hears when it asks for
/// less rather than a session that quietly reaches everything.
#[tokio::test]
async fn a_host_local_session_reaches_what_the_host_does() {
    use cortex::console::NetworkAccess;

    // Asked for nothing: the session every client had before there was a reach to name, now
    // answered with what it actually has.
    let console = built_with(None)
        .await
        .expect("a session that asked nothing");
    assert_eq!(console.network().map(|n| n.reach.as_str()), Some("full"));
    drop(console);

    // Asking for what it has is fine. Asking for less is refused, and named.
    for (reach, allowed) in [
        ("full", true),
        ("public", false),
        ("host", false),
        ("none", false),
    ] {
        match built_with(Some(NetworkAccess::new(reach))).await {
            Ok(_) => assert!(allowed, "{reach} was answered and cannot be"),
            Err(e) => {
                assert!(!allowed, "{reach} was refused: {e}");
                let failure = e
                    .downcast_ref::<cortex::console::Failure>()
                    .expect("a protocol failure");
                assert_eq!(
                    failure.code(),
                    Some(Error::UNSUPPORTED_NETWORK),
                    "{reach} was refused as {failure:?}, which is a different problem"
                );
            }
        }
    }
}

/// A console over the real binary with no tree, asking for `reach` or for nothing.
async fn built_with(reach: Option<cortex::console::NetworkAccess>) -> anyhow::Result<Console> {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-local-console"));
    server.stderr(Stdio::inherit());

    let mut builder = Console::builder().client(StdioClient::new(server)?);
    if let Some(reach) = reach {
        builder = builder.network(reach);
    }
    builder.build().await
}

/// **A command that writes without limit shortens its own answer and not the session.**
///
/// A response travels in one frame under `MAX_PAYLOAD`, so output that would not fit is a
/// frame this protocol refuses to send — and a server that tried would lose the channel
/// mid-answer, taking every later call with it. So the streams are cut, `truncated` says
/// they were, and the session carries on: measured here by asking it something afterwards.
#[tokio::test]
async fn a_command_that_writes_too_much_is_cut_and_the_session_survives() {
    let mut fx = Fixture::new().await;

    // Comfortably over `MAX_PAYLOAD`, so an uncut answer could not have been sent at all.
    let flood = fx
        .console
        .exec(
            ["sh", "-c", "yes aaaaaaaaaaaaaaaa | head -c 70000000"],
            None,
        )
        .await
        .expect("an answer rather than a broken channel");

    assert!(flood.truncated, "the cut is reported");
    assert!(
        flood.stdout.len() < 64 * 1024 * 1024,
        "cut to something one frame holds: {} bytes",
        flood.stdout.len()
    );
    assert!(flood.stdout.iter().all(|b| *b == b'a' || *b == b'\n'));

    // The point of the whole thing: the channel is still there.
    assert_eq!(fx.output("echo alive").await.stdout, b"alive\n");
}
