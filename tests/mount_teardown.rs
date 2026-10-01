//! Taking a mount down while something else is going on: a second mount alive, or the process
//! killed. That is how a console runs: a mount per session, sessions overlapping, eventually
//! stopped with a signal.
//!
//! ```sh
//! cargo test --test mount_teardown -- --ignored --nocapture
//! ```
//!
//! On Linux `fuser` mounts through `mount(2)` itself and needs no libfuse, so a container is
//! enough, given `--device /dev/fuse` and `--cap-add SYS_ADMIN`.
//!
//! One set of bodies for both unix bindings, since [`Mount`]'s contract is what is under test.
//! A binding's library may assume one mount per process (libfuse-t keeps the FUSE-T helper's
//! pid in a single process-global slot), so teardowns run with a second mount alive.

#![cfg(all(feature = "mount", unix))]

use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

// Whichever host binding this target has.
#[cfg(not(target_os = "macos"))]
use virtx::fs::FuseMount as HostMount;
#[cfg(target_os = "macos")]
use virtx::fs::FuseTMount as HostMount;
use virtx::fs::{FileSystem, InMemFs, Mount};

/// How long a teardown gets before the test calls it a hang.
///
/// Generous (a working teardown takes tens of milliseconds): it separates "slow"
/// from "never", and a loaded machine blurs a tighter bound.
const TEARDOWN_DEADLINE: Duration = Duration::from_secs(20);

fn mountpoint(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("virtx-mount-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir is writable");
    dir
}

fn volume() -> InMemFs {
    let vol = InMemFs::new();
    let rt = tokio::runtime::Runtime::new().expect("build a runtime for volume setup");
    rt.block_on(async {
        let greeting = std::path::Path::new("greeting.txt");
        vol.create(greeting).await.expect("fresh store");
        vol.write_at(greeting, b"Hello from virtx!\n", 0)
            .await
            .expect("write the greeting");
    });
    vol
}

/// Read through the mount, so it asserts the mount *serves*, not merely exists.
fn assert_serves(mount: &impl Mount) {
    assert_eq!(
        fs::read_to_string(mount.mountpoint().join("greeting.txt")).unwrap(),
        "Hello from virtx!\n"
    );
}

/// Run `body` on a thread and fail if it has not finished within the deadline.
///
/// A hung teardown parks in a syscall rather than panicking, so only a deadline
/// catches it. An overrunning thread is left behind: it cannot be cancelled, and
/// the process is about to exit.
fn within_deadline(what: &str, body: impl FnOnce() + Send + 'static) {
    let done = Arc::new(Barrier::new(2));
    let signal = Arc::clone(&done);
    std::thread::spawn(move || {
        body();
        signal.wait();
    });

    // `Barrier` has no timed wait, so the deadline is polled on a channel instead.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        done.wait();
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(TEARDOWN_DEADLINE).is_ok(),
        "{what} did not finish within {TEARDOWN_DEADLINE:?} — it is wedged, not slow"
    );
}

/// Two mounts alive at once, taken down one after the other on one thread.
///
/// Not about concurrency: one thread, drops in source order. What matters is that
/// `b` was mounted while `a` was up.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn two_overlapping_mounts_come_down_in_the_order_they_are_dropped() {
    let (pa, pb) = (mountpoint("overlapA"), mountpoint("overlapB"));
    let a = HostMount::try_new(volume(), &pa).expect("mount a");
    let b = HostMount::try_new(volume(), &pb).expect("mount b");
    assert_serves(&a);
    assert_serves(&b);

    // Oldest first: the order in which `a`'s teardown could wait on `b`'s helper.
    // The reverse would pass regardless.
    within_deadline("dropping the older of two live mounts", move || drop(a));
    within_deadline("dropping the remaining mount", move || drop(b));

    for p in [&pa, &pb] {
        assert!(
            !is_mounted(p),
            "{} is still in the mount table after its guard was dropped",
            p.display()
        );
        let _ = fs::remove_dir_all(p);
    }
}

/// Two mounts whose teardowns are started at the same instant, on threads of
/// their own.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn two_mounts_taken_down_at_the_same_time_both_come_down() {
    const MOUNTS: usize = 2;
    let gate = Arc::new(Barrier::new(MOUNTS));
    let mut threads = Vec::new();

    for i in 0..MOUNTS {
        let gate = Arc::clone(&gate);
        threads.push(std::thread::spawn(move || {
            let path = mountpoint(&format!("parallel{i}"));
            let mount = HostMount::try_new(volume(), &path).expect("mount");
            assert_serves(&mount);
            // Both mounts are up before either teardown starts, so they overlap.
            gate.wait();
            drop(mount);
            path
        }));
    }

    let deadline = Instant::now() + TEARDOWN_DEADLINE;
    let mut paths = Vec::new();
    for (i, thread) in threads.into_iter().enumerate() {
        // `JoinHandle` has no timed join, so the assertion is on elapsed time.
        loop {
            if thread.is_finished() {
                paths.push(
                    thread
                        .join()
                        .unwrap_or_else(|_| panic!("mount {i} panicked")),
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "teardown {i} did not finish within {TEARDOWN_DEADLINE:?} — it is wedged"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    for p in &paths {
        assert!(
            !is_mounted(p),
            "{} is still in the mount table after its guard was dropped",
            p.display()
        );
        let _ = fs::remove_dir_all(p);
    }
}

/// A mount taken down while the kernel is still asking it for things.
///
/// A request in flight at the drop races its reply against the channel being taken
/// apart; wrong ordering trips an assertion inside libfuse-t
/// (`fuse_kern_chan_send`: "se != NULL", `SIGABRT`) and aborts the process.
///
/// Readers fail once the mount goes, so their results are not asserted; only that
/// the process survives.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_mount_dropped_under_load_does_not_abort() {
    const READERS: usize = 4;
    let path = mountpoint("underload");
    let mount = HostMount::try_new(volume(), &path).expect("mount");
    assert_serves(&mount);

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let (file, stop) = (path.join("greeting.txt"), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // Reads and listings, so several opcodes are in flight.
                    let _ = fs::read_to_string(&file);
                    let _ = fs::read_dir(file.parent().expect("the mount point"));
                }
            })
        })
        .collect();

    // Long enough for the readers to be mid-request rather than starting up.
    std::thread::sleep(Duration::from_millis(200));
    within_deadline("dropping a mount under load", move || drop(mount));

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for reader in readers {
        reader.join().expect("a reader must not panic");
    }
    assert!(!is_mounted(&path), "the mount came down under load");
    let _ = fs::remove_dir_all(&path);
}

// ---------------------------------------------------------------------------
// Signals: the exits that run no destructor at all.
// ---------------------------------------------------------------------------

/// How long a child gets to put its mount up before the test gives up on it.
const CHILD_MOUNT_DEADLINE: Duration = Duration::from_secs(20);

/// The env var naming where the child fixture should mount.
const CHILD_MOUNTPOINT: &str = "VIRTX_CHILD_MOUNTPOINT";

/// Set when the child should call [`virtx::fs::unmount_on_signal`] first.
const CHILD_CATCHES_SIGNALS: &str = "VIRTX_CHILD_CATCHES_SIGNALS";

/// Mount where [`CHILD_MOUNTPOINT`] says, then wait to be killed.
///
/// A fixture: signal tests re-run this binary with the variable set, since a
/// signal that ends a process cannot be tested from inside it. Unset, it does
/// nothing.
#[test]
#[ignore = "a fixture: the signal tests run it as a child process"]
fn child_mounts_and_waits_to_be_killed() {
    let Ok(path) = std::env::var(CHILD_MOUNTPOINT) else {
        return;
    };
    if std::env::var_os(CHILD_CATCHES_SIGNALS).is_some() {
        virtx::fs::unmount_on_signal().expect("install the signal teardown");
    }
    let _mount = HostMount::try_new(volume(), std::path::Path::new(&path)).expect("mount");

    // The parent watches the mount table and kills us; this never returns.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// A running child fixture, killed when it goes out of scope.
///
/// `std::process::Child` does not kill on drop, and the fixture waits forever, so
/// a panicking test would otherwise leave a child holding a mount.
struct Fixture(std::process::Child);

impl Fixture {
    /// Send `signal` and wait for the child to die.
    ///
    /// `libc::kill`, since `kill(1)` is missing from some Linux base images.
    fn kill_and_reap(&mut self, signal: libc::c_int) {
        // SAFETY: the child was spawned here and has not been reaped, so the pid is
        // ours and still names it.
        let sent = unsafe { libc::kill(self.0.id() as libc::pid_t, signal) };
        assert_eq!(sent, 0, "signal {signal} was refused");
        self.0.wait().expect("reap the child");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Errors mean a test already killed it.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn the fixture above on `path` and return it once its mount is up.
fn child_holding_a_mount(path: &std::path::Path, catches_signals: bool) -> Fixture {
    let mut command = std::process::Command::new(
        std::env::current_exe().expect("the test binary knows its own path"),
    );
    command
        .args([
            "--exact",
            "child_mounts_and_waits_to_be_killed",
            "--ignored",
        ])
        .env(CHILD_MOUNTPOINT, path)
        // Otherwise the child's output pollutes the next failing test's capture.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if catches_signals {
        command.env(CHILD_CATCHES_SIGNALS, "1");
    }
    // Guarded now, so the wait below can fail without leaking the child.
    let child = Fixture(command.spawn().expect("spawn the child fixture"));

    // The mount table, not `exists`: `stat` hangs on the abandoned mount.
    let deadline = Instant::now() + CHILD_MOUNT_DEADLINE;
    while !in_mount_table(path) {
        assert!(
            Instant::now() < deadline,
            "the child did not mount {} within {CHILD_MOUNT_DEADLINE:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    child
}

/// `SIGKILL` reaches no handler, so the next mount reclaims it.
///
/// The state between the kill and the next mount is not asserted: any mount in
/// this process (including a parallel test's) sweeps, so only the end state is
/// the contract.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_killed_process_leaves_a_mount_that_the_next_mount_reclaims() {
    let abandoned = mountpoint("killed");
    let mut child = child_holding_a_mount(&abandoned, false);
    child.kill_and_reap(libc::SIGKILL);

    // Any mount anywhere: the sweep is over the register, not this path.
    let elsewhere = mountpoint("killed-probe");
    let probe = HostMount::try_new(volume(), &elsewhere).expect("mount");

    assert!(
        !in_mount_table(&abandoned),
        "{} outlived the process that made it and was not reclaimed",
        abandoned.display()
    );
    drop(probe);
    let _ = fs::remove_dir_all(&abandoned);
    let _ = fs::remove_dir_all(&elsewhere);
}

/// Poll the mount table until `path` is gone from it, or fail saying `why`.
fn comes_down(path: &std::path::Path, why: &str) {
    let deadline = Instant::now() + TEARDOWN_DEADLINE;
    while in_mount_table(path) {
        assert!(
            Instant::now() < deadline,
            "{} was still mounted {TEARDOWN_DEADLINE:?} after {why}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// After `SIGKILL` the mount comes down with nothing mounting again, via the
/// claim's watchdog outside the process.
///
/// Meaningful only with `--test-threads=1`: a parallel test's mount would sweep
/// the path too.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_killed_process_takes_its_mount_down_with_nobody_mounting_again() {
    let path = mountpoint("killed-watched");
    let mut child = child_holding_a_mount(&path, false);
    child.kill_and_reap(libc::SIGKILL);
    comes_down(&path, "its process was killed");
    let _ = fs::remove_dir_all(&path);
}

/// A process without [`virtx::fs::unmount_on_signal`] ended by an uncaught
/// `SIGTERM` still loses its mount, via the watchdog.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_process_that_did_not_opt_in_unmounts_when_it_is_asked_to_stop() {
    let path = mountpoint("terminated");
    let mut child = child_holding_a_mount(&path, false);
    child.kill_and_reap(libc::SIGTERM);
    comes_down(&path, "its process was sent SIGTERM");
    let _ = fs::remove_dir_all(&path);
}

/// Reclaiming asks whether the *owner* is gone, not whether the path is one it
/// recognises.
///
/// Automatic reclaim is safe only because of this: sweeping by path would take down
/// a second instance's tree whenever the first mounted.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn reclaiming_leaves_a_running_process_its_own_mounts() {
    let path = mountpoint("livesibling");
    let mount = HostMount::try_new(volume(), &path).expect("mount");
    assert_serves(&mount);

    virtx::fs::reclaim_abandoned();

    // This process is running, so its mount is not abandoned.
    assert_serves(&mount);
    drop(mount);
    let _ = fs::remove_dir_all(&path);
}

/// A caught signal takes the mount with it, once the program opted in.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_process_that_opted_in_unmounts_when_it_is_asked_to_stop() {
    let path = mountpoint("signalled");
    let mut child = child_holding_a_mount(&path, true);

    child.kill_and_reap(libc::SIGTERM);

    // The process dies at the re-raise, so the unmount may still be in flight when
    // `wait` returns; assert it happens, not that it has.
    let deadline = Instant::now() + TEARDOWN_DEADLINE;
    while in_mount_table(&path) {
        assert!(
            Instant::now() < deadline,
            "{} was still mounted {TEARDOWN_DEADLINE:?} after the process was asked to stop",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = fs::remove_dir_all(&path);
}

/// Whether the mount table names `path`, asked of `mount(8)`.
///
/// Not `stat`, which hangs on a mount whose server is gone; `mount` reads the
/// kernel's list and always returns. The library's own reader is not public, since
/// consumers name what they own, never a path.
fn in_mount_table(path: &std::path::Path) -> bool {
    // The table spells paths resolved (macOS `/var` is a symlink to `private/var`).
    // Only the parent is resolved: `canonicalize` on an abandoned mount hangs.
    let resolved = match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .map(|parent| parent.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    };

    let out = std::process::Command::new("mount")
        .output()
        .expect("`mount` lists the table on both hosts these tests run on");
    let table = String::from_utf8_lossy(&out.stdout);
    // ` on <path> ` is where every `mount` output puts the mount point. No test path
    // has a space.
    let needle = format!(" on {} ", resolved.display());
    table.lines().any(|line| line.contains(&needle))
}

/// Whether something is mounted at `path`: its device id differs from its parent's.
///
/// `stat`, not the mount table, so it agrees with what a process walking the path
/// sees.
fn is_mounted(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else {
        return false;
    };
    match (fs::metadata(path), fs::metadata(parent)) {
        (Ok(here), Ok(above)) => here.dev() != above.dev(),
        _ => false,
    }
}
