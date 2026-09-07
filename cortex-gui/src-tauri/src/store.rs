//! Making a resource's store real: mount the workspace, run `mem init` / `index init` against
//! it, unmount.
//!
//! # Why a mount, and why only for a moment
//!
//! SQLite opens a file. Not a path in a `WorkFs` — a file, on a filesystem some kernel answers
//! for, with a sibling journal it can create and locks it can take. `mem` and `index` are the
//! programs that write a store, they take a path on the command line, and nothing about them
//! knows this process exists. So the only way a store lands *inside* the workspace is for the
//! workspace to be somewhere a kernel answers for while the program runs.
//!
//! That is what this does, and it does it for the length of one command: mount at a directory
//! under the working directory, run the one `init`, take the mount down. Holding a mount for the
//! whole session would be the same code with the guard kept, and is very likely where this ends
//! up — an agent that runs commands needs one anyway. It is not that yet because a mount that
//! outlives a failure is a directory somebody has to go and `umount` by hand, and a scoped one
//! cannot be left behind.
//!
//! # What it depends on, and why that is temporary
//!
//! Two binaries this window does not contain: `cortex-local-console`, which is the console
//! server that runs a command on this host, and `mem`/`index`, which is the command. Both are
//! built by the workspace above, so [`bin_dir`] finds them the way a developer would — the
//! nearest `target/debug` or `target/release` — and `CORTEX_BIN_DIR` overrides it. A bundled app
//! cannot look for a `target` directory, so shipping them as Tauri sidecars is what replaces
//! this; the seam is one function.
//!
//! The store format was already designed for this. `cortex_exec_storebase::sqlite` stays on
//! SQLite's rollback journal *because* a store may sit on a FUSE mount — WAL needs a
//! shared-memory file every connection `mmap`s coherently, which no FUSE filesystem owes anyone
//! — so what a store needs here is the ability to make a sibling file, which the workspace has.

use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

use cortex::{console::Console, fs::FuseTMount, fs::WorkFs};
use tokio::sync::RwLock;

use crate::{
    error::{Error, Result},
    shared::SharedFs,
    state::ResourceKind,
};

/// Where the workspace is mounted while a store is being made, relative to the working
/// directory.
///
/// Under the working directory rather than a temporary directory, because this is the one place
/// the window reaches outside itself and a person debugging it should be able to find it. Dotted,
/// created on demand, and removed after — a mount point has to exist and be empty, so one left
/// behind is the thing that stops the next attempt.
const MOUNT_DIR: &str = ".cortex-mnt";

/// How long an `init` gets. It creates one small file; a minute of it means the mount is wedged,
/// not that SQLite is slow.
const INIT_TIMEOUT_MS: u64 = 60_000;

/// How many times, and how far apart, removing the mount point is tried. See [`tidy`].
const TIDY_ATTEMPTS: usize = 10;
const TIDY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Create the store for a resource, inside the workspace.
///
/// Returns whatever the program said on stdout, which is what the window shows.
pub async fn init_store(
    fs: Arc<RwLock<WorkFs>>,
    kind: ResourceKind,
    store_path: &str,
) -> Result<String> {
    let program = executable(kind)?;
    let console_bin = bin_dir()?.join(CONSOLE_BIN);
    let mountpoint = env::current_dir()?.join(MOUNT_DIR);
    // Absolute, because a session names its tree as a `file://` URL and a relative path after
    // `file://` reads as a host. `current_dir` is what makes it so.
    tokio::fs::create_dir_all(&mountpoint).await?;
    if is_mounted(&mountpoint) {
        return Err(Error::msg(format!(
            "{} 에 이미 마운트된 것이 있습니다 — 앞선 실행이 정리되지 않았습니다. \
             umount {} 후 다시 시도해 주세요.",
            mountpoint.display(),
            mountpoint.display()
        )));
    }

    let shared = SharedFs::new(fs);
    let at = mountpoint.clone();
    // `try_new` returns only once the kernel has attached the mount, which it establishes by
    // polling with sleeps in between — so it cannot run on a runtime worker.
    let mount = tokio::task::spawn_blocking(move || FuseTMount::try_new(shared, &at))
        .await
        .map_err(|err| Error::msg(format!("마운트 스레드가 죽었습니다: {err}")))?
        .map_err(|err| {
            Error::msg(format!(
                "{} 에 마운트하지 못했습니다: {err}\n\
                 FUSE-T 가 필요합니다 (brew install --cask fuse-t). \
                 이전 실행이 남겨둔 마운트라면 umount {} 후 다시 시도해 주세요.",
                mountpoint.display(),
                mountpoint.display()
            ))
        })?;

    let outcome = run_init(&console_bin, &program, store_path, mount).await;
    tidy(&mountpoint).await;
    outcome
}

/// Remove the mount point, if the kernel has let go of it yet.
///
/// **Not on the first try.** Dropping the guard joins the thread that served the mount, which is
/// this process finished with it — the kernel detaching it is a moment later, and a `rmdir` in
/// between answers `EBUSY`. So this retries briefly and then leaves the directory alone: an
/// empty one is harmless, the next attempt reuses it, and a *mounted* one is caught by the
/// pre-flight check above with something useful to say. What must not happen is a failure here
/// being reported as a failure of the `init` that already succeeded.
async fn tidy(mountpoint: &Path) {
    for _ in 0..TIDY_ATTEMPTS {
        if tokio::fs::remove_dir(mountpoint).await.is_ok() {
            return;
        }
        tokio::time::sleep(TIDY_INTERVAL).await;
    }
}

/// Whether something is mounted at `path` — the unix answer, a device id that differs from its
/// parent's, which holds whatever the mount is. That matters for FUSE-T, whose transport is its
/// own choice and none of whose three options is a FUSE mount.
fn is_mounted(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let Some(parent) = path.parent() else {
        return false;
    };
    match (std::fs::metadata(path), std::fs::metadata(parent)) {
        (Ok(here), Ok(above)) => here.dev() != above.dev(),
        _ => false,
    }
}

/// The console server this window drives.
const CONSOLE_BIN: &str = "cortex-local-console";

/// One console, one command, and the mount goes down with it.
async fn run_init(
    console_bin: &Path,
    program: &Path,
    store_path: &str,
    mount: FuseTMount,
) -> Result<String> {
    let mut console = Console::builder()
        .stdio_client(&[console_bin.to_string_lossy()])
        .mount(mount)
        .build()
        .await
        .map_err(|err| {
            Error::msg(format!(
                "{} 를 시작하지 못했습니다: {err}",
                console_bin.display()
            ))
        })?;

    // Relative, so it resolves where the session stands — the root of the mounted workspace.
    // An absolute path would name this host's own root instead, which is where a store nobody
    // asked for would appear.
    let relative = store_path.trim_start_matches('/');
    let outcome = console
        .exec(
            [program.to_string_lossy().as_ref(), "init", relative],
            Some(INIT_TIMEOUT_MS),
        )
        .await
        .map_err(|err| Error::msg(format!("{} init: {err}", program.display())));

    // Dropping the console says `quit` and then takes the mount down, and taking a mount down
    // joins the thread serving it — blocking work, which does not belong on a runtime worker.
    let _ = tokio::task::spawn_blocking(move || drop(console)).await;

    let response = outcome?;
    if response.code != 0 {
        let said = String::from_utf8_lossy(&response.stderr);
        let said = said.trim();
        return Err(Error::msg(if said.is_empty() {
            format!(
                "{} init 이 {} 로 끝났습니다",
                program.display(),
                response.code
            )
        } else {
            said.to_string()
        }));
    }
    Ok(String::from_utf8_lossy(&response.stdout).trim().to_string())
}

/// The program that writes a store of this kind.
fn executable(kind: ResourceKind) -> Result<PathBuf> {
    let name = match kind {
        ResourceKind::Memory => "mem",
        ResourceKind::Docset => "index",
    };
    Ok(bin_dir()?.join(name))
}

/// The directory holding the executables this window shells out to.
///
/// `CORTEX_BIN_DIR` if it is set, otherwise the nearest `target/debug` or `target/release`
/// walking up from the working directory — which is how a developer running `npm run tauri dev`
/// from `cortex-gui` finds the workspace's own build. Everything it needs has to be in one
/// directory, because `cargo build` puts it there.
fn bin_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("CORTEX_BIN_DIR") {
        let dir = PathBuf::from(dir);
        return if complete(&dir) {
            Ok(dir)
        } else {
            Err(Error::msg(format!(
                "CORTEX_BIN_DIR={} 에 {CONSOLE_BIN}, mem, index 가 모두 있어야 합니다",
                dir.display()
            )))
        };
    }

    let mut here = env::current_dir()?;
    loop {
        for profile in ["debug", "release"] {
            let candidate = here.join("target").join(profile);
            if complete(&candidate) {
                return Ok(candidate);
            }
        }
        if !here.pop() {
            break;
        }
    }
    Err(Error::msg(format!(
        "{CONSOLE_BIN}, mem, index 를 찾지 못했습니다. \
         워크스페이스에서 `cargo build -p cortex-local-console -p cortex-exec-mem -p \
         cortex-exec-index` 를 실행하거나 CORTEX_BIN_DIR 을 지정해 주세요."
    )))
}

/// Whether `dir` holds every executable this module needs.
///
/// All three, not the one a given call wants: a directory with `mem` and no `index` is a partial
/// build, and finding it would make "add a docset" fail with a path error from two directories
/// up rather than with the build instruction above.
fn complete(dir: &Path) -> bool {
    [CONSOLE_BIN, "mem", "index"]
        .iter()
        .all(|name| dir.join(name).is_file())
}

#[cfg(test)]
mod tests {
    use cortex::fs::{FileSystem, InMemFs};

    use super::*;
    use crate::fsops::mkdir_p;

    /// The chain this module exists for, end to end: mount the workspace, let `mem` write a
    /// store into it as an ordinary file on an ordinary path, unmount, and find the store in the
    /// tree afterwards.
    ///
    /// `#[ignore]` because it needs what a machine either has or has not, and neither is
    /// something a test should quietly skip over: FUSE-T installed, and the workspace above
    /// built. Run it with
    ///
    /// ```text
    /// cargo build -p cortex-local-console -p cortex-exec-mem -p cortex-exec-index   # ../..
    /// cargo test -- --ignored --nocapture --test-threads=1                          # here
    /// ```
    ///
    /// `--test-threads=1` because there is one mount point and a mount point has to be empty:
    /// two of these at once is the collision [`Workspace::store_init`] serializes in the window,
    /// and the test harness has no such lock.
    ///
    /// [`Workspace::store_init`]: crate::state::Workspace::store_init
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn mem_init_writes_a_store_into_the_workspace() {
        let fs = Arc::new(RwLock::new(
            WorkFs::new()
                .try_with_mount("", InMemFs::new())
                .expect("the root is a valid mount path"),
        ));
        let store = "/.cortex/memory/notes.sqlite";
        {
            // The directory a manifest would have made. `mem init` creates the file, never its
            // parent — a filesystem's `create` resolves only the parent, and this is that.
            let guard = fs.read().await;
            mkdir_p(&*guard, Path::new("/.cortex/memory"))
                .await
                .unwrap();
        }

        let said = init_store(fs.clone(), ResourceKind::Memory, store)
            .await
            .expect("mem init through a mounted workspace");
        println!("mem said: {said:?}");

        let guard = fs.read().await;
        let stat = guard
            .stat(Path::new(store))
            .await
            .expect("the store is there");
        // Not merely created: SQLite wrote the schema through the mount and the bytes came back
        // to the in-memory store underneath it.
        assert!(stat.size > 0, "an empty file is a `create`, not an `init`");
        println!("store size: {} bytes", stat.size);
    }
}
