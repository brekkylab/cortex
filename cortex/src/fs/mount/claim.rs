//! Which mount points this process owns — in memory for the signal path, on disk
//! for the run that comes after this one.
//!
//! A guard is the owner of a mount while it lives, and both things that outlive a
//! guard need to know what it owned. The signal handler needs it *now*, in this
//! address space. [`reclaim_abandoned`] needs it in a **later process**, because
//! the run that has to clear a `SIGKILL`ed mount is by definition not the run that
//! made it. One claim covers both: taking one records the mount point in memory
//! and in a file, dropping it removes both.
//!
//! # Why a register at all, rather than reading the mount table
//!
//! The mount table cannot say which mounts are cortex's. A FUSE-T mount is spelled
//! `nfs` there, and the mount point is a path the *caller* chose — cortex takes an
//! arbitrary `&Path` and has no naming convention to recognise later. So what is
//! swept has to be what was written down.
//!
//! # Why the pid, and what it costs
//!
//! “Abandoned” is not “under a root I know about” — it is “owned by a process that
//! is gone”. Without that test a second instance of a program with a fixed mount
//! point would unmount the first instance's *live* tree. The pid in the file name
//! is what makes the test possible: `kill(pid, 0)` answering `ESRCH` is the one
//! answer that means abandoned, where `EPERM` means alive under another user.
//!
//! A pid can be reused, and then a dead owner looks alive and its mount is left for
//! a later run to meet again. That is the safe direction to be wrong in: the
//! mistake is failing to reclaim, never reclaiming something still in use.

use std::{
    ffi::OsString,
    fs, io,
    os::unix::{
        ffi::OsStringExt,
        fs::{DirBuilderExt, MetadataExt},
    },
    path::PathBuf,
    sync::Mutex,
};

// Only taking a claim needs these, and only a build with a binding can take one.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
use {
    super::table::resolved,
    std::{
        os::unix::ffi::OsStrExt,
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
    },
};

use super::table::{mounts_under, unmount_under};

/// Where the records live, under the temporary directory.
const DIR: &str = "cortex-mounts";

/// How many abandoned mount points one call will spend an unmount on.
///
/// Each costs a bounded wait, and this runs on the thread that is trying to mount,
/// so an unbounded scan would make a host's accumulated wreckage everybody's
/// startup latency. Whatever is left over is the next run's to try.
///
/// Only mounts that were actually there are charged; a record whose mount is
/// already gone costs a `read` and a `remove_file`.
const BUDGET: usize = 4;

/// Distinguishes two mounts made by one process — a pid alone does not.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Every mount this process has up, resolved as the mount table spells them.
///
/// Written by every guard, whether or not a signal handler was ever installed: a
/// consumer that calls [`unmount_on_signal`](super::unmount_on_signal) *after*
/// mounting has to find the mounts that already exist, and a register that only
/// started counting at installation would miss exactly those.
static LIVE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// What this process currently has mounted.
pub(crate) fn live() -> Vec<PathBuf> {
    // Poisoning is ignored: a panic elsewhere must not turn every later mount into
    // an unrecoverable one.
    LIVE.lock().map(|live| live.clone()).unwrap_or_default()
}

/// A guard's ownership of one mount point, held for as long as the mount is.
///
/// Gated on there being a binding to claim with: with none compiled in, nothing in
/// this process can mount. [`reclaim_abandoned`] stays ungated — clearing what a
/// *previous* run left needs no binding at all, only the register and a path.
///
/// Dropping it gives the mount point up in both registers, so an ordinary teardown
/// leaves nothing for the signal path or for a later run to find.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
pub(crate) struct Claim {
    mountpoint: PathBuf,

    /// The record on disk. `None` when there was nowhere trustworthy to write one,
    /// which costs recovery-after-`SIGKILL` and nothing else — the mount still
    /// works and still comes down on drop.
    record: Option<PathBuf>,
}

/// Record `mountpoint` as this process's, in memory and on disk.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
pub(crate) fn claim(mountpoint: &Path) -> Claim {
    let mountpoint = resolved(mountpoint);
    if let Ok(mut live) = LIVE.lock() {
        live.push(mountpoint.clone());
    }

    let record = registry().map(|dir| {
        let name = format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(name);
        // Bytes, not a string: a mount point is a path and need not be UTF-8, and
        // one that is not is still a path that has to come down.
        let _ = fs::write(&path, mountpoint.as_os_str().as_bytes());
        path
    });
    Claim { mountpoint, record }
}

#[cfg(any(feature = "fuse", feature = "fuse-t"))]
impl Drop for Claim {
    fn drop(&mut self) {
        // Dropped unconditionally, unlike the record below: this is the list a signal
        // unmounts, and an entry no guard stands behind is a path this process would
        // take down whatever ends up mounted there later.
        if let Ok(mut live) = LIVE.lock()
            && let Some(at) = live.iter().position(|p| p == &self.mountpoint)
        {
            live.swap_remove(at);
        }
        // The record outlives the guard when the mount does, so a later run meets it
        // again rather than forgetting a mount that is still in the way. Both exits
        // that report a mount they could not take down reach this with it still up.
        //
        // Asked of the mount table rather than passed in: the guard knows what it
        // attempted, the table knows what is there.
        if let Some(record) = &self.record
            && mounts_under(&self.mountpoint).is_empty()
        {
            let _ = fs::remove_file(record);
        }
    }
}

/// The directory the records live in, created if it is not there.
///
/// `None` rather than an error, and `None` for anything the least bit wrong: this
/// directory decides what a later run will `umount`, so a version of it somebody
/// else can write into is a way to be told to unmount an arbitrary path. Mode
/// `0700` and owned by us, or it is not used.
///
/// The check is not redundant with the mode passed to `create`: a directory that
/// already exists keeps the mode and the owner it already had, and on Linux
/// `/tmp` is world-writable, so anyone could have made this one first.
fn registry() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(DIR);
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&dir)
        .ok()?;

    let meta = fs::metadata(&dir).ok()?;
    // SAFETY: `geteuid` reads a process-global id and cannot fail.
    if meta.uid() != unsafe { libc::geteuid() } {
        return None;
    }
    if meta.mode() & 0o022 != 0 {
        return None;
    }
    Some(dir)
}

/// Whether `pid` is gone.
///
/// `kill` with signal 0 sends nothing and only reports whether the pid could be
/// signalled. `ESRCH` — no such process — is the one answer that means the mount
/// is abandoned; `EPERM` says it is alive under another user, and that is not ours
/// to take down.
fn gone(pid: libc::pid_t) -> bool {
    // Nothing but a real process id is asked about. `kill` reads 0 as "every
    // process in my group" and a negative number as "the group named by its
    // absolute value", so a record whose name parsed to one of those would ask a
    // question about *ourselves* and be answered "alive" — never reclaimed, but by
    // accident rather than by rule.
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs only the permission and existence check; it cannot
    // affect this or any other process.
    let answer = unsafe { libc::kill(pid, 0) };
    answer == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Take down every mount this host has that belonged to a cortex process which is
/// no longer running, and report what that met.
///
/// This is the `SIGKILL` half of the contract. Nothing a dying process does can
/// help there — no handler runs — so the mount point is cleared by whoever comes
/// next, which is what this is. A binding's `try_new` calls it, so an ordinary
/// consumer gets it without asking.
///
/// **Call it directly when something has to touch the mount point before mounting.**
/// A leftover mount that nothing answers makes `stat` on that path block, so a
/// `create_dir_all` under it hangs *before* `try_new` is ever reached. Sweeping
/// first is what stops that, and needs no argument: what to reclaim is on the
/// register, not in a path the caller has to name.
///
/// Only mounts owned by a **dead** process are touched, which is what makes it safe
/// to call unasked: a live sibling instance keeps its mounts however its paths are
/// spelled. Sweeping by *path* could not promise that, which is why no such call is
/// offered.
///
/// Answers with what is still mounted after the attempt — empty when everything
/// abandoned came down, and otherwise what a person has to clear by hand.
///
/// Bounded: at most four abandoned mounts are unmounted per call, each with
/// its own deadline. Never panics, never fails — a host with nothing to reclaim
/// costs one directory read.
pub fn reclaim_abandoned() -> Vec<PathBuf> {
    let mut left = Vec::new();
    let Some(dir) = registry() else {
        return left;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return left;
    };

    let mut budget = BUDGET;
    for entry in entries.flatten() {
        let record = entry.path();
        let named = entry
            .file_name()
            .to_str()
            .and_then(|name| name.split('-').next()?.parse::<libc::pid_t>().ok());
        // Every record this writes is named for a real pid, so one that is not is
        // something an older version or a stray hand left. Nothing can be asked
        // about its owner, which means keeping it is keeping it forever.
        let Some(pid) = named.filter(|pid| *pid > 0) else {
            let _ = fs::remove_file(&record);
            continue;
        };
        if !gone(pid) {
            continue;
        }

        // Unreadable or empty: the owner is gone and the record says nothing, so
        // there is nothing to unmount and no reason to keep it.
        let Ok(bytes) = fs::read(&record) else {
            let _ = fs::remove_file(&record);
            continue;
        };
        if bytes.is_empty() {
            let _ = fs::remove_file(&record);
            continue;
        }
        let mountpoint = PathBuf::from(OsString::from_vec(bytes));
        // Asked before the budget is consulted, because a record whose mount is
        // already gone costs one syscall to be rid of. Letting a spent budget stop
        // *that* would leave the registry growing behind a backlog it is not part
        // of, and would make what gets cleared depend on the order a directory
        // happens to be read in.
        if mounts_under(&mountpoint).is_empty() {
            let _ = fs::remove_file(&record);
            continue;
        }
        if budget == 0 {
            // The rest is the next run's. Better than making this mount wait on a
            // backlog somebody else's crashes left.
            break;
        }

        budget -= 1;
        if unmount_under(&mountpoint) {
            let _ = fs::remove_file(&record);
        } else {
            // The record is kept, so a later run meets this again rather than
            // forgetting a mount that is still in the way.
            left.push(mountpoint);
        }
    }
    left
}

#[cfg(all(test, any(feature = "fuse", feature = "fuse-t")))]
mod tests {
    use super::*;

    /// The register is what the signal path reads and what a later run sweeps, so
    /// a mount that is up has to be on it and one that is gone has to be off it.
    #[test]
    fn a_claim_lasts_exactly_as_long_as_it_is_held() {
        let path = std::env::temp_dir().join("cortex-claim-probe");
        let resolved = resolved(&path);

        // This path and no other. The register is process-wide and these tests run
        // beside each other, so its *total* length is somebody else's business too.
        let held = claim(&path);
        assert!(live().contains(&resolved), "the register takes the path");
        let record = held.record.clone().expect("a record was written");
        assert!(
            record.exists(),
            "the record is on disk while the claim is held"
        );

        drop(held);
        assert!(!live().contains(&resolved), "the register lets the path go");
        assert!(!record.exists(), "the record goes with the claim");
    }

    /// Our own pid is alive, so our own records must never be reclaimed — the
    /// property that keeps a second instance from unmounting the first's tree.
    #[test]
    fn a_live_process_keeps_its_own_records() {
        let path = std::env::temp_dir().join("cortex-claim-live-probe");
        let held = claim(&path);
        let record = held.record.clone().expect("a record was written");

        reclaim_abandoned();
        assert!(
            record.exists(),
            "a record whose owner is running is not somebody else's to clear"
        );
        drop(held);
    }

    /// Both guards have an exit reached with the mount still up, and after it the
    /// record is all that lets a later run meet that mount again. Dropping the
    /// claim must not be what forgets it.
    #[test]
    #[ignore = "mounts a real filesystem"]
    fn a_record_outlives_a_claim_dropped_over_a_live_mount() {
        #[cfg(all(feature = "fuse", not(feature = "fuse-t")))]
        use crate::fs::FuseMount as HostMount;
        #[cfg(feature = "fuse-t")]
        use crate::fs::FuseTMount as HostMount;
        use crate::fs::{FileSystem, InMemFs};

        let path =
            std::env::temp_dir().join(format!("cortex-claim-live-mount-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp dir is writable");

        let volume = InMemFs::new();
        let rt = tokio::runtime::Runtime::new().expect("a runtime to seed the volume");
        rt.block_on(async { volume.create(Path::new("greeting.txt")).await })
            .expect("fresh store");
        let mount = HostMount::try_new(volume, &path).expect("the volume mounts");

        // A second claim on the same point, so dropping it leaves the mount up —
        // which is the state the guards' failure exits hand to `Claim::drop`.
        let held = claim(&path);
        let record = held.record.clone().expect("a record was written");
        drop(held);
        assert!(
            record.exists(),
            "a record whose mount is still up is what a later run reclaims"
        );

        drop(mount);
        let _ = fs::remove_file(&record);
        let _ = fs::remove_dir_all(&path);
    }

    /// A record left by a pid that cannot exist is abandoned by definition, and
    /// naming no mount it costs nothing to clear.
    #[test]
    fn a_record_from_a_dead_owner_is_cleared() {
        let Some(dir) = registry() else {
            panic!("the registry is usable under $TMPDIR");
        };
        // Above the largest pid any of these systems hands out, so it names nothing
        // running and cannot come to name something later.
        let record = dir.join(format!("{}-cortex-test", libc::pid_t::MAX));
        let mountpoint = std::env::temp_dir().join("cortex-claim-dead-probe");
        fs::create_dir_all(&mountpoint).unwrap();
        fs::write(&record, mountpoint.as_os_str().as_bytes()).unwrap();

        reclaim_abandoned();
        assert!(!record.exists(), "an abandoned record is cleared");
        fs::remove_dir_all(&mountpoint).ok();
    }
}
