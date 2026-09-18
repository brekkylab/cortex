//! What a mount leaves behind in the *process*, which is not supposed to be anything.
//!
//! Every other test about mounting asks what is at the mount point. These ask the
//! opposite question: after a mount, is the process the caller had before it? A
//! signal disposition is process-global and shared with everything else in the
//! program, so a library that changes one as a side effect of mounting is changing
//! state that was never its to change, and the damage lands nowhere near the
//! mount.
//!
//! `SIGCHLD` is the one that matters, because that is how a process learns its
//! children have exited. `tokio::process::Child::wait` is built on it. So a mount
//! that replaces the handler does not break the mount: it breaks every `wait` the
//! program makes from then on, in a part of the code that never heard of a
//! filesystem, with the child having already done its work and exited cleanly.
//! That is the failure these tests exist to keep out.
//!
//! `#[ignore]`, because both bodies mount a real filesystem:
//!
//! ```sh
//! # macOS + FUSE-T
//! PKG_CONFIG_PATH="$PWD/cortex/contrib/pkgconfig:/usr/local/lib/pkgconfig" \
//!     cargo test --features fuse-t --test mount_sigchld -- --ignored --nocapture
//!
//! # Linux, where `fuser` mounts through `mount(2)` itself and needs no libfuse:
//! cargo test --features fuse --test mount_sigchld -- --ignored --nocapture
//! ```
//!
//! One set of bodies for both host bindings, as in `host_mount.rs` and
//! `mount_teardown.rs`: leaving the process alone is part of what mounting owes,
//! so both bindings owe it and neither is the one under test.

#![cfg(any(feature = "fuse", feature = "fuse-t"))]

use std::{fs, path::PathBuf, time::Duration};

#[cfg(all(feature = "fuse", not(feature = "fuse-t")))]
use cortex::fs::FuseMount as HostMount;
#[cfg(feature = "fuse-t")]
use cortex::fs::FuseTMount as HostMount;
use cortex::fs::InMemFs;

/// How long a `wait` on a child that has already exited may take and still count
/// as having been reaped by the notification.
///
/// The child here is `true`, which is done in single-digit milliseconds. Seconds
/// rather than milliseconds anyway, because what is being told apart is "reaped"
/// and "not reaped", and a loaded machine makes the first look slow without making
/// it wrong.
const REAP_BUDGET: Duration = Duration::from_secs(3);

/// How long the test waits before giving up on the `wait` entirely.
///
/// Only so that a broken run ends. **The assertion is on [`REAP_BUDGET`], not on
/// this**, and deliberately: a `timeout` that expires wakes the task, and the poll
/// that wakeup causes reaps the child through `try_wait` without any notification
/// having arrived. So a `wait` that was never going to return on its own still
/// comes back `Ok` here, at the deadline and not before. Timing it is what tells
/// the two apart.
const GIVE_UP: Duration = Duration::from_secs(15);

fn mountpoint(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("cortex-sigchld-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir is writable");
    dir
}

/// A `SIGCHLD` disposition, reduced to the two fields that say who handles it.
///
/// Not the whole `sigaction`: `sa_mask` is a `sigset_t`, which is an opaque array
/// on Linux and not comparable without going through `sigismember` for every
/// signal there is. The handler and the flags are what a replacement changes and
/// what this needs to see.
#[derive(Debug, PartialEq, Eq)]
struct Disposition {
    handler: usize,
    flags: libc::c_int,
}

impl Disposition {
    /// What the process currently does with `SIGCHLD`.
    ///
    /// A null `act` asks without setting, which is the only way to read one.
    fn read() -> Self {
        // SAFETY: `current` is written by `sigaction` before it is read, and the
        // null `act` means nothing is installed.
        let current = unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut current),
                0,
                "reading a signal disposition cannot fail for a valid signal"
            );
            current
        };
        Self {
            handler: current.sa_sigaction,
            flags: current.sa_flags,
        }
    }

    /// How this reads in a failure message: the named constants where there is
    /// one, and the address otherwise, since a handler is only ever compared for
    /// having changed.
    fn describe(&self) -> String {
        let handler = match self.handler {
            h if h == libc::SIG_DFL => "SIG_DFL".to_string(),
            h if h == libc::SIG_IGN => "SIG_IGN".to_string(),
            h => format!("a handler at {h:#x}"),
        };
        format!("{handler}, flags {:#x}", self.flags)
    }
}

/// A `SIGCHLD` handler installed before a mount is still installed after it.
///
/// The narrow form of the claim, and the one that says *what* went wrong rather
/// than only that something did: no runtime, no children, nothing to race with.
///
/// A handler is installed first, and that is the whole setup rather than a
/// convenience. With nothing installed the disposition is `SIG_DFL` before the
/// mount and `SIG_DFL` after it, and a mount that resets the disposition looks
/// exactly like one that leaves it alone. What is under test is a *replacement*,
/// so there has to be something there to replace.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_mount_leaves_sigchld_alone() {
    let _handler = Handler::install();

    let before = Disposition::read();
    assert_ne!(
        before.handler,
        libc::SIG_DFL,
        "the fixture did not take: there is nothing here for a mount to overwrite"
    );

    let mount = HostMount::try_new(InMemFs::new(), &mountpoint("disposition"))
        .expect("the host binding can mount");
    let during = Disposition::read();

    drop(mount);
    let after = Disposition::read();

    assert_eq!(
        before,
        during,
        "mounting replaced the process's SIGCHLD handling: it was {}, and the mount \
         made it {}. Every `wait` this process makes from here on is answering to \
         whatever that is.",
        before.describe(),
        during.describe()
    );
    assert_eq!(
        before,
        after,
        "unmounting replaced the process's SIGCHLD handling: it was {}, and the \
         teardown made it {}.",
        before.describe(),
        after.describe()
    );
}

/// A `SIGCHLD` handler for the length of a test, put back on the way out.
///
/// Standing in for what a runtime installs. Nothing observes it running, and the
/// body is empty on purpose: the handler exists in order to *be* one, so that the
/// disposition is something other than `SIG_DFL` and a replacement is visible as
/// one.
///
/// Restored on drop because a disposition is process-global, and a test that left
/// one behind would be deciding what the next test in this binary sees.
struct Handler(libc::sigaction);

impl Handler {
    fn install() -> Self {
        extern "C" fn noticed(_: libc::c_int) {}

        // SAFETY: `action` is fully initialized below, and `was` is written by
        // `sigaction` before it is read.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = noticed as *const () as usize;
            action.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
            libc::sigemptyset(&mut action.sa_mask);

            let mut was: libc::sigaction = std::mem::zeroed();
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, &action, &mut was),
                0,
                "installing a SIGCHLD handler"
            );
            Self(was)
        }
    }
}

impl Drop for Handler {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the value `sigaction` itself wrote when this was
        // installed, so it is a disposition the process had.
        unsafe { libc::sigaction(libc::SIGCHLD, &self.0, std::ptr::null_mut()) };
    }
}

/// A child that exits after a mount is still reaped.
///
/// The consequence, exercised the way a program meets it. The first child is what
/// makes tokio install its `SIGCHLD` handler, which it does lazily on the first
/// spawn: before that there is nothing for a mount to overwrite, which is why a
/// program that mounts at startup never sees this and one that remounts does.
///
/// The mount is between the two spawns on purpose, and is taken on this thread
/// rather than inside the runtime because that is where a caller takes one.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_child_is_still_reaped_after_a_mount() {
    let rt = tokio::runtime::Runtime::new().expect("build a runtime");

    // Not an assertion about mounting: this is the setup, and it is the step that
    // arms the failure. Without a child before the mount, tokio has registered
    // nothing and the mount has nothing to break.
    rt.block_on(async {
        tokio::process::Command::new("true")
            .spawn()
            .expect("spawn `true`")
            .wait()
            .await
            .expect("a child that exits is reaped");
    });

    let _mount = HostMount::try_new(InMemFs::new(), &mountpoint("reap"))
        .expect("the host binding can mount");

    let started = std::time::Instant::now();
    let reaped = rt.block_on(async {
        let mut child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn `true` again");
        tokio::time::timeout(GIVE_UP, child.wait()).await
    });

    let took = started.elapsed();

    reaped
        .expect("the wait gave no sign of returning on its own")
        .expect("waiting on a child that exited cleanly");
    assert!(
        took < REAP_BUDGET,
        "a child that had already exited took {took:?} to be reaped after a mount, \
         which is the wait not returning until something else woke it. SIGCHLD is \
         now {}: the mount took the notification the runtime waits on.",
        Disposition::read().describe()
    );
}

/// Two sessions in one process, one after the other.
///
/// The shape a long-running program actually has, and the question the two tests
/// above do not answer: whether the *first* mount protects what comes after it. It
/// does not. A process that mounts before it spawns anything is safe only until
/// its next mount, because what the first session installs is what the second
/// session's mount takes away.
///
/// `true` stands in for a console server here. What matters is not what the child
/// does but that a runtime waited on one before the second mount, which is the step
/// that arms this.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_second_session_does_not_break_the_first_session_s_waits() {
    let rt = tokio::runtime::Runtime::new().expect("build a runtime");

    // Session one, in the safe order: mount first, spawn second.
    let first = HostMount::try_new(InMemFs::new(), &mountpoint("session-one"))
        .expect("the host binding can mount");
    rt.block_on(async {
        tokio::process::Command::new("true")
            .spawn()
            .expect("spawn `true`")
            .wait()
            .await
            .expect("a child that exits is reaped");
    });
    drop(first);

    // Session two. Nothing here is out of order: this mount is the first thing the
    // session does. It is only the second one the *process* has made.
    let _second = HostMount::try_new(InMemFs::new(), &mountpoint("session-two"))
        .expect("the host binding can mount again");

    let started = std::time::Instant::now();
    let reaped = rt.block_on(async {
        let mut child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn `true` in the second session");
        tokio::time::timeout(GIVE_UP, child.wait()).await
    });
    let took = started.elapsed();

    reaped
        .expect("the wait gave no sign of returning on its own")
        .expect("waiting on a child that exited cleanly");
    assert!(
        took < REAP_BUDGET,
        "the second session's wait took {took:?}. Mounting once at startup did not \
         protect the process: the second mount took the notification back out. \
         SIGCHLD is now {}.",
        Disposition::read().describe()
    );
}
