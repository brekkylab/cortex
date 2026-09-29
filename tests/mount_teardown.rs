//! Taking a mount down — the half of a guard's contract that only fails when
//! something else is going on at the same time.
//!
//! The bodies in `host_mount.rs` each hold one mount for the length of one test
//! and let it go before the next begins, so they say nothing about a *second*
//! mount being up at the time, or about a process that is killed while one is.
//! Both of those are how a console actually runs: a mount per session, sessions
//! overlapping, and the whole thing eventually stopped with a signal.
//!
//! `#[ignore]`, because every body here mounts a real filesystem:
//!
//! ```sh
//! cargo test --test mount_teardown -- --ignored --nocapture
//! ```
//!
//! On Linux `fuser` mounts through `mount(2)` itself and needs no libfuse, so a
//! container is enough, given `--device /dev/fuse` and `--cap-add SYS_ADMIN`.
//!
//! One set of bodies for both host bindings, as in `host_mount.rs` and for the
//! same reason: what is under test is [`Mount`]'s contract, which both owe, so
//! running them against either is evidence the two behave *alike* rather than
//! that one behaves.
//!
//! # Overlapping lifetimes are their own case
//!
//! A binding is a third-party library over a kernel interface, and "one mount per
//! process" is an assumption any of them may hold — libfuse-t does, keeping the
//! FUSE-T helper's pid in a single process-global slot. So a teardown is exercised
//! with a second mount alive, which is a different case from a teardown on its own
//! and the one a console meets: a mount per session, sessions overlapping.
//!
//! Two mounts alive at once is the whole requirement. Threads are not part of it,
//! which is why one of these runs on a single thread.

#![cfg(all(feature = "mount", unix))]

use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

// Whichever host binding this target has, as in `host_mount.rs`.
#[cfg(not(target_os = "macos"))]
use cortex::fs::FuseMount as HostMount;
#[cfg(target_os = "macos")]
use cortex::fs::FuseTMount as HostMount;
use cortex::fs::{FileSystem, InMemFs, Mount};

/// How long a teardown gets before the test calls it a hang.
///
/// Generous next to a teardown that works — those measure in tens of
/// milliseconds — because the point is to tell "slow" from "never", and a loaded
/// machine makes the first look like the second at a tighter bound.
const TEARDOWN_DEADLINE: Duration = Duration::from_secs(20);

fn mountpoint(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("cortex-mount-{}-{}", std::process::id(), tag));
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
        vol.write_at(greeting, b"Hello from cortex!\n", 0)
            .await
            .expect("write the greeting");
    });
    vol
}

/// Read through the mount, so the assertion is that it *serves* and not merely
/// that something is attached at the path.
fn assert_serves(mount: &impl Mount) {
    assert_eq!(
        fs::read_to_string(mount.mountpoint().join("greeting.txt")).unwrap(),
        "Hello from cortex!\n"
    );
}

/// Run `body` on a thread and fail if it has not finished within the deadline.
///
/// A hung teardown is not a panic — the thread parks in a syscall and stays there
/// — so nothing but a deadline can turn one into a test failure. The thread is
/// left behind rather than joined when it overruns: it cannot be cancelled, and
/// the report matters more than the tidiness of a process that is about to exit
/// anyway.
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
/// The minimal form of the case, and the one that shows it is not about
/// concurrency: a single thread, drops in source order. What matters is only that
/// `b` was mounted while `a` was still up.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn two_overlapping_mounts_come_down_in_the_order_they_are_dropped() {
    let (pa, pb) = (mountpoint("overlapA"), mountpoint("overlapB"));
    let a = HostMount::try_new(volume(), &pa).expect("mount a");
    let b = HostMount::try_new(volume(), &pb).expect("mount b");
    assert_serves(&a);
    assert_serves(&b);

    // Dropped oldest first, which is the order that pins `a`'s teardown on `b`'s
    // helper. The reverse order happens to work even unfixed, so testing it
    // would prove nothing.
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
            // Both mounts are up before either teardown starts, so the two
            // teardowns genuinely overlap rather than merely being on threads.
            gate.wait();
            drop(mount);
            path
        }));
    }

    let deadline = Instant::now() + TEARDOWN_DEADLINE;
    let mut paths = Vec::new();
    for (i, thread) in threads.into_iter().enumerate() {
        // `JoinHandle` cannot be joined with a timeout, so the deadline is read
        // off the clock: a wedged teardown never finishes, so the assertion has
        // to be about elapsed time rather than about the join.
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
/// An idle teardown and a busy one are different tests: a request in flight at the
/// moment of the drop is a reply racing the channel being taken apart, and that is
/// the only condition under which either binding has to get the ordering right.
///
/// Getting it wrong aborts the process rather than failing a call: a reply on its
/// way out through a channel that is being taken apart trips an assertion inside
/// libfuse-t (`fuse_kern_chan_send`: "se != NULL", `signal: 6, SIGABRT`).
///
/// The readers are expected to start failing the instant the mount goes — that is
/// what unmounting does to them — so nothing here asserts on what they read. What
/// is asserted is that the process is still alive to run the assertion at all.
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
                    // Reads and directory listings both, so more than one opcode
                    // is in flight when the channel goes.
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
const CHILD_MOUNTPOINT: &str = "CORTEX_CHILD_MOUNTPOINT";

/// Set when the child should call [`cortex::fs::unmount_on_signal`] first.
const CHILD_CATCHES_SIGNALS: &str = "CORTEX_CHILD_CATCHES_SIGNALS";

/// Mount where [`CHILD_MOUNTPOINT`] says, then wait to be killed.
///
/// A fixture and not a test: the two below run this binary again with the
/// variable set, because a signal that ends a process cannot be tested from
/// inside the process it ends. With the variable unset — which is how a plain
/// sweep of the suite runs it — there is nothing to do and it does nothing.
#[test]
#[ignore = "a fixture: the signal tests run it as a child process"]
fn child_mounts_and_waits_to_be_killed() {
    let Ok(path) = std::env::var(CHILD_MOUNTPOINT) else {
        return;
    };
    if std::env::var_os(CHILD_CATCHES_SIGNALS).is_some() {
        cortex::fs::unmount_on_signal().expect("install the signal teardown");
    }
    let _mount = HostMount::try_new(volume(), std::path::Path::new(&path)).expect("mount");

    // The parent watches the mount table rather than this process, so there is
    // nothing to announce. It kills us; nothing here returns.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// A running child fixture, killed when it goes out of scope.
///
/// `std::process::Child` does not kill on drop, and the fixture waits forever on
/// purpose — so a test that panics before killing its own child would leave one
/// behind holding a mount, turning one failed assertion into a wedged path on the
/// machine.
struct Fixture(std::process::Child);

impl Fixture {
    /// Send `signal` and wait for the child to die.
    ///
    /// `libc::kill` rather than the `kill(1)` binary, which is not in every base
    /// image a Linux run happens in.
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
        // Both already done when a test killed it itself; the errors say so and are
        // not worth reporting from a destructor.
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
        // Inherited otherwise, and a child reporting its own progress into the
        // parent's captured output is noise in whichever test fails next.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if catches_signals {
        command.env(CHILD_CATCHES_SIGNALS, "1");
    }
    // Held from here on, so the wait below can fail without leaking the child.
    let child = Fixture(command.spawn().expect("spawn the child fixture"));

    // The mount table and not `exists`: the child's mount is about to be abandoned
    // by the tests below, and a `stat` on one of those is the hang, not an answer.
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

/// `SIGKILL` reaches no handler, so the mount outlives the process — and the only
/// place left to deal with it is whoever comes next. Nobody has to ask: mounting
/// is what reclaims it.
///
/// Nothing is asserted about the state *between* the kill and the next mount. Any
/// mount in this process sweeps, including one made by a test running beside this
/// one, so the middle is racy by design and the end state is what the contract is
/// about: after something mounts, an abandoned mount is gone.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_killed_process_leaves_a_mount_that_the_next_mount_reclaims() {
    let abandoned = mountpoint("killed");
    let mut child = child_holding_a_mount(&abandoned, false);
    child.kill_and_reap(libc::SIGKILL);

    // Any mount at all, anywhere — the sweep is over the register, not over this
    // path, so nothing here has to name what it is reclaiming.
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

/// `SIGKILL` runs no code of the process's, and the mount still comes down -- without
/// anything mounting again, because the claim's watchdog is outside the process.
///
/// Run on its own (`--test-threads=1`) to mean what it says: a mount made by a test
/// beside it sweeps the register too, and would clear the path the same way.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_killed_process_takes_its_mount_down_with_nobody_mounting_again() {
    let path = mountpoint("killed-watched");
    let mut child = child_holding_a_mount(&path, false);
    child.kill_and_reap(libc::SIGKILL);
    comes_down(&path, "its process was killed");
    let _ = fs::remove_dir_all(&path);
}

/// A process that never asked for [`cortex::fs::unmount_on_signal`] and is ended by a
/// signal it does not catch -- `SIGTERM`, as a supervisor sends -- takes its mount with
/// it all the same: nothing in the process runs, and the watchdog does it.
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
/// The whole safety of doing this automatically rests on that. A sweep that went by
/// path alone would take down a second instance's tree the moment the first instance
/// mounted, which is why sweeping by path is not something a consumer can ask for.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn reclaiming_leaves_a_running_process_its_own_mounts() {
    let path = mountpoint("livesibling");
    let mount = HostMount::try_new(volume(), &path).expect("mount");
    assert_serves(&mount);

    cortex::fs::reclaim_abandoned();

    // This process is running, so its mount is not abandoned, whatever the register
    // says about the path.
    assert_serves(&mount);
    drop(mount);
    let _ = fs::remove_dir_all(&path);
}

/// A signal that *can* be caught takes the mount with it — once the program has
/// asked for that.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_process_that_opted_in_unmounts_when_it_is_asked_to_stop() {
    let path = mountpoint("signalled");
    let mut child = child_holding_a_mount(&path, true);

    child.kill_and_reap(libc::SIGTERM);

    // The handler hands off to a thread, and the process dies as soon as that
    // thread re-raises — so the unmount can still be in flight when `wait`
    // returns. What is asserted is that it happens, not that it already has.
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
/// A `stat` cannot be used for this. Half these tests are about a mount whose server
/// is gone, and a `stat` on one of those does not answer — it hangs, which is the
/// whole problem under test. `mount` reads the kernel's list instead and always
/// comes back.
///
/// The library reads that list too, and better, but not for anyone outside it: what
/// a consumer may take down is what it owns, never a path it names, so there is no
/// public reader to borrow here. Shelling out is the honest way for a test to look.
fn in_mount_table(path: &std::path::Path) -> bool {
    // The table spells the path the long way, so the short one matches no row: macOS
    // `$TMPDIR` is `/var/folders/…` and `/var` is a symlink to `private/var`. Only
    // the *parent* is resolved — `path` itself is a mount point these tests
    // deliberately abandon, and `canonicalize` on one of those is the hang.
    //
    // The library resolves the same way and keeps it to itself, which is the price of
    // a public surface that names only what a consumer owns.
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
    // ` on <path> ` — the one field every `mount` spelling puts the mount point in.
    // No test path here has a space in it.
    let needle = format!(" on {} ", resolved.display());
    table.lines().any(|line| line.contains(&needle))
}

/// Whether the OS has something mounted at `path`, decided the unix way — a
/// directory whose device id differs from its parent's.
///
/// Asked of `stat` rather than of the mount table because it must agree with
/// what a process walking the path would see, which is the thing a leftover
/// mount breaks.
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
