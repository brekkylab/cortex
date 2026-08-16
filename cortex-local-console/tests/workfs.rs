//! End-to-end over the real binary: the tree a session works in, and where it stands in it.
//!
//! Both are things only the far end can settle — it is the process that answers `init`,
//! spawns the commands and holds the directory between them — so the only way to see what
//! this backend does with them is to drive it over the channel and ask.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use cortex::BoxFuture;
use cortex::console::stdio::StdioClient;
use cortex::console::{Console, Error, ExecResult};
use cortex::executable::{ExecCall, ExecResult as ExecOutput, Executable, ExecutableSet};
use cortex::fs::Mount;
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

    async fn output(&mut self, script: &str) -> ExecResult {
        self.console
            .exec(["sh", "-c", script], None)
            .await
            .expect("running the command")
    }

    /// `cd`, which this backend answers itself rather than spawning.
    async fn cd(&mut self, args: &[&str]) -> ExecResult {
        let mut cmd = vec!["cd"];
        cmd.extend_from_slice(args);
        self.console.exec(cmd, None).await.expect("running cd")
    }
}

async fn console_over(root: &Path) -> anyhow::Result<Console> {
    let mut server = Command::new(env!("CARGO_BIN_EXE_cortex-local-console"));
    server.stderr(Stdio::inherit());
    let client = StdioClient::new(server)?;

    Console::builder()
        .client(client)
        .mount(Mounted(root.to_path_buf()))
        .executables(ExecutableSet::new().register(
            "cat-here",
            "read a file out of the tree, where the command stood",
            CatHere,
        ))
        .build()
        .await
}

/// Answers with the contents of the file it was named, opened through the mount it was
/// handed — the client's half of one file name meaning one file.
struct CatHere;

impl Executable for CatHere {
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecOutput> {
        Box::pin(async move {
            let Some(mount) = mount else {
                return ExecOutput::failed(1, "nothing is mounted");
            };
            let path = match call.resolve(&call.args[0]) {
                Ok(path) => mount.host_path(&path),
                Err(e) => return ExecOutput::failed(1, format!("{}: {e}", call.args[0])),
            };
            match std::fs::read(&path) {
                Ok(bytes) => ExecOutput::ok(bytes),
                Err(e) => ExecOutput::failed(1, format!("{}: {e}", path.display())),
            }
        })
    }
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

/// `cd` moves the session and says where to, and the command after it runs there.
///
/// It is not a program — no `cd` exists on `PATH` — so this is the server answering as the
/// shell a person would be talking to, which is the only place a console session's
/// directory could live.
#[tokio::test]
async fn cd_moves_the_session_and_the_next_command_runs_there() {
    let mut fx = Fixture::new().await;
    std::fs::create_dir(fx.path("work")).unwrap();

    let moved = fx.cd(&["work"]).await;
    assert_eq!(moved.code, 0);
    assert_eq!(moved.cwd.as_deref(), fx.path("work").to_str());

    assert_eq!(
        fx.output("pwd").await.stdout,
        format!("{}\n", fx.path("work").display()).into_bytes()
    );

    // Relative from where it now stands, and `..` gets back — resolved by the server, so
    // what comes back is a path with nothing left to resolve.
    let back = fx.cd(&[".."]).await;
    assert_eq!(back.cwd.as_deref(), fx.root.to_str());

    // And `cd` with nothing is the tree, the way a shell's is `$HOME`.
    fx.cd(&["work"]).await;
    assert_eq!(fx.cd(&[]).await.cwd.as_deref(), fx.root.to_str());
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
    assert_eq!(out.cwd, None, "and said nothing about the session");

    assert_eq!(
        fx.output("pwd").await.stdout,
        format!("{}\n", fx.root.display()).into_bytes(),
    );
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
    assert_eq!(refused.cwd, None, "nothing moved, so nothing to report");

    assert_eq!(
        fx.output("pwd").await.stdout,
        format!("{}\n", fx.root.display()).into_bytes()
    );
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

    assert_eq!(
        fx.output("pwd").await.stdout,
        format!("{}\n", fx.path("work").display()).into_bytes()
    );
}

/// The whole loop, which is the thing the tree exists for: a command standing somewhere
/// invokes a delegated name with a relative argument, and the name opens the file the
/// command meant.
///
/// Four hops and two spellings of the same directory — the shim reports the server's own
/// path, the client strips the tree's root off it, `resolve` joins the argument onto what
/// is left, and the mount turns that back into a file. A `cd` first, so the directory being
/// carried is not the root and a bug that substituted one would show.
#[tokio::test]
async fn a_delegated_name_opens_the_file_the_command_meant() {
    let mut fx = Fixture::new().await;
    std::fs::create_dir(fx.path("work")).unwrap();
    std::fs::write(fx.path("work/note.txt"), b"from the tree\n").unwrap();
    std::fs::write(fx.path("note.txt"), b"from the root\n").unwrap();

    fx.cd(&["work"]).await;

    // Relative: resolved against where the command stood, which is the directory the `cd`
    // moved the session to.
    assert_eq!(
        fx.output("cat-here note.txt").await.stdout,
        b"from the tree\n"
    );

    // And `..` out of it names the other one, because the directory it pops is one the
    // command was standing in.
    assert_eq!(
        fx.output("cat-here ../note.txt").await.stdout,
        b"from the root\n"
    );
}

/// **Every path the server reports is spelled the way the one it answered at `init` is.**
///
/// A mount point reached through a symlink is the ordinary case, and the two spellings of
/// it are not interchangeable to a client: it was told one, and a `cd` or a reported
/// directory that came back in the other would be a path it has no way to relate to
/// anything. `getcwd(2)` answers the physical one, so this is something the server has to
/// put right rather than something that holds by itself.
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

    // `cd`: normalized on paper, not canonicalized.
    assert_eq!(
        fx.cd(&["work"]).await.cwd.as_deref(),
        fx.path("work").to_str()
    );

    // And a delegated call, whose directory came out of `getcwd(2)` in another process —
    // the client can only strip the root off it because it arrived re-spelled.
    std::fs::write(fx.path("work/note.txt"), b"resolved\n").unwrap();
    assert_eq!(fx.output("cat-here note.txt").await.stdout, b"resolved\n");
}

/// A delegated call carries both halves of its context at once — where the command stood
/// and what it was running with — and a command that moved itself first is reported where
/// it actually is rather than where the session stands.
#[tokio::test]
async fn a_delegated_call_carries_the_directory_and_the_environment_together() {
    let mut fx = Fixture::new().await;
    std::fs::create_dir(fx.path("work")).unwrap();
    std::fs::write(fx.path("work/note.txt"), b"in work\n").unwrap();
    std::fs::write(fx.path("note.txt"), b"at the root\n").unwrap();

    // The session stands at the root; the command walks into `work` on its own and the
    // delegated name is answered from *there*, because the shim reports where it stood and
    // not where the session does.
    let out = fx.output("cd work && FOO=bar cat-here note.txt").await;
    assert_eq!(out.stdout, b"in work\n");
    assert_eq!(out.cwd, None, "the session did not move");

    // Which is the file the command itself would have opened by the same name.
    assert_eq!(
        fx.output("cd work && cat note.txt").await.stdout,
        b"in work\n"
    );
}
