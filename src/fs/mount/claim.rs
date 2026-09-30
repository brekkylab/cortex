//! Which mount points this process owns: in memory for the signal path, on disk for later
//! runs, since a `SIGKILL`ed mount can only be cleared by another run. Taking a claim records
//! the mount point in both places; dropping it removes both, the disk one only once the mount
//! is down.
//!
//! A register rather than the mount table, which cannot say which mounts are cortex's: a
//! FUSE-T mount is spelled `nfs` there, and the mount point is an arbitrary caller-chosen path.
//! Each record is named for its owner's pid, so abandoned means "owner gone" ([`gone`]).

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
#[cfg(feature = "mount")]
use {
    super::table::resolved,
    std::{
        os::unix::ffi::OsStrExt,
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
    },
};

use super::table::{mounts_under, unmount_under};

/// Records directory, under the temporary directory.
const DIR: &str = "cortex-mounts";

/// How many abandoned mounts one call will try to unmount.
///
/// Each costs a bounded wait on the thread trying to mount, so an unbounded sweep would turn
/// a host's accumulated leftovers into startup latency. The rest is the next run's. Only
/// mounts still present are charged.
const BUDGET: usize = 4;

/// Distinguishes two mounts made by one process — a pid alone does not.
#[cfg(feature = "mount")]
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Every mount this process has up, resolved as the mount table spells them.
///
/// Kept even with no signal handler installed, so
/// [`unmount_on_signal`](super::unmount_on_signal) called after mounting still sees them.
static LIVE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// What this process currently has mounted.
pub(crate) fn live() -> Vec<PathBuf> {
    // Poisoning ignored, so one panic does not make every later mount unrecoverable.
    LIVE.lock().map(|live| live.clone()).unwrap_or_default()
}

/// A guard's ownership of one mount point, held for as long as the mount is.
///
/// Gated on `mount`, since without a binding nothing can mount.
#[cfg(feature = "mount")]
pub(crate) struct Claim {
    mountpoint: PathBuf,

    /// The record on disk. `None` when there was nowhere trustworthy to write one, which
    /// costs only recovery after `SIGKILL`.
    record: Option<PathBuf>,
}

/// Record `mountpoint` as this process's, in memory and on disk.
#[cfg(feature = "mount")]
pub(crate) fn claim(mountpoint: &Path) -> Claim {
    let mountpoint = resolved(mountpoint);
    if let Ok(mut live) = LIVE.lock() {
        live.push(mountpoint.clone());
    }

    let record = registry().map(|dir| {
        watch(&dir);
        let name = format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(name);
        // Bytes, since a mount point need not be UTF-8.
        let _ = fs::write(&path, mountpoint.as_os_str().as_bytes());
        path
    });
    Claim { mountpoint, record }
}

/// The watchdog's script (see [`watch`]).
///
/// `cat` returns once no process holds the pipe's write end; owner pid and registry come in
/// the environment. Stop signals are ignored so it outlives the process they were sent to.
#[cfg(feature = "mount")]
const WATCHDOG_SCRIPT: &str = r#"
trap '' INT TERM HUP QUIT PIPE
cat >/dev/null
for record in "$CORTEX_MOUNT_REGISTRY/$CORTEX_MOUNT_OWNER"-*; do
    [ -f "$record" ] || continue
    mountpoint=$(cat "$record") || continue
    if [ -z "$mountpoint" ]; then rm -f "$record"; continue; fi
    if [ "$(uname)" = Darwin ]; then
        umount "$mountpoint" 2>/dev/null || diskutil unmount force "$mountpoint" >/dev/null 2>&1
    else
        umount "$mountpoint" 2>/dev/null || umount -l "$mountpoint" 2>/dev/null
    fi && rm -f "$record"
done
"#;

/// Start this process's watchdog, once per pid.
///
/// Covers exits that run none of this process's code: `SIGKILL`, a crash, the OOM killer, or a
/// runtime that skips finalizers (Node on `process.exit()`). The watchdog is `/bin/sh` in its
/// own process group (so a terminal's `^C` misses it), reading a pipe whose write end only
/// this process holds. However this process ends, the kernel closes that end and the watchdog
/// unmounts whatever this process's records still name (plain `umount`, then force), removing
/// a record only once its mount is down so the rest stays for [`reclaim_abandoned`]. After an
/// ordinary exit it finds no records.
///
/// Per pid because a forked child inherits the parent's write end, which would keep the
/// parent's watchdog waiting on the child too; a child that mounts replaces it with its own.
///
/// Best effort: without a watchdog the next run's reclaim clears the mount instead.
#[cfg(feature = "mount")]
fn watch(registry: &Path) {
    use std::{
        os::unix::process::CommandExt,
        process::{ChildStdin, Command, Stdio},
    };

    static WATCHDOG: Mutex<Option<(u32, ChildStdin)>> = Mutex::new(None);

    let pid = std::process::id();
    let Ok(mut watchdog) = WATCHDOG.lock() else {
        return;
    };
    if watchdog.as_ref().is_some_and(|(owner, _)| *owner == pid) {
        return;
    }
    let spawned = Command::new("/bin/sh")
        .args(["-c", WATCHDOG_SCRIPT, "cortex-mount-watchdog"])
        .env("CORTEX_MOUNT_OWNER", pid.to_string())
        .env("CORTEX_MOUNT_REGISTRY", registry)
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    // Never waited on: the watchdog outlives this process by design. The write end is
    // `CLOEXEC`, so nothing this process spawns holds it.
    if let Ok(mut child) = spawned
        && let Some(stdin) = child.stdin.take()
    {
        *watchdog = Some((pid, stdin));
    }
}

#[cfg(feature = "mount")]
impl Drop for Claim {
    fn drop(&mut self) {
        // Always removed: a signal unmounts this list, and a stale entry would take down
        // whatever gets mounted there later.
        if let Ok(mut live) = LIVE.lock()
            && let Some(at) = live.iter().position(|p| p == &self.mountpoint)
        {
            live.swap_remove(at);
        }
        // Kept while the mount is still up (a failed teardown), so a later run reclaims it.
        // Asked of the mount table, which knows what is actually there.
        if let Some(record) = &self.record
            && mounts_under(&self.mountpoint).is_empty()
        {
            let _ = fs::remove_file(record);
        }
    }
}

/// The directory the records live in, created if it is not there.
///
/// `None` unless it is ours and not group/world-writable: its contents decide what a later
/// run will `umount`, so a writable one lets anyone request unmounting an arbitrary path.
///
/// Checked after `create` because an existing directory keeps its mode and owner, and on
/// Linux anyone could have made it first in world-writable `/tmp`.
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
/// Only `ESRCH` from `kill(pid, 0)` means gone; `EPERM` is alive under another user.
///
/// A reused pid makes a dead owner look alive, so its mount waits for a later run: failing to
/// reclaim, never reclaiming something in use.
fn gone(pid: libc::pid_t) -> bool {
    // `kill` reads 0 and negatives as process groups, which would ask about ourselves.
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks existence and permission.
    let answer = unsafe { libc::kill(pid, 0) };
    answer == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Take down every mount left by a cortex process that is no longer running.
///
/// Covers `SIGKILL`, where no handler runs and only the next run can help; the dead
/// process's watchdog usually took its mounts down already, and this gets what it could not.
/// Every unix binding's `try_new` calls it.
///
/// **Call it directly before touching a mount point ahead of mounting:** `stat` blocks on a
/// leftover mount nothing answers, so a `create_dir_all` under it would hang before `try_new`.
///
/// Only a **dead** owner's mounts are touched, which is what makes calling it automatically
/// safe: a live sibling instance keeps its mounts. A path says nothing about whose mount is on
/// it, so no sweep-by-path call exists.
///
/// Returns the mounts it tried and failed to take down, which a person must clear by hand;
/// ones past the budget are left for the next call and not listed.
///
/// A bounded number of unmounts per call (`BUDGET`), each with its own deadline. Never panics
/// or fails; with nothing to reclaim it costs one directory read.
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
        // Records are named for a real pid; one that is not has no owner to ask about, so
        // keeping it would be forever.
        let Some(pid) = named.filter(|pid| *pid > 0) else {
            let _ = fs::remove_file(&record);
            continue;
        };
        if !gone(pid) {
            continue;
        }

        // Unreadable or empty: nothing to unmount.
        let Ok(bytes) = fs::read(&record) else {
            let _ = fs::remove_file(&record);
            continue;
        };
        if bytes.is_empty() {
            let _ = fs::remove_file(&record);
            continue;
        }
        let mountpoint = PathBuf::from(OsString::from_vec(bytes));
        // Before the budget check: clearing a stale record is cheap, and a spent budget
        // must not let the registry grow or make cleanup depend on directory order.
        if mounts_under(&mountpoint).is_empty() {
            let _ = fs::remove_file(&record);
            continue;
        }
        if budget == 0 {
            // The rest is the next run's.
            break;
        }

        budget -= 1;
        if unmount_under(&mountpoint) {
            let _ = fs::remove_file(&record);
        } else {
            // Kept so a later run tries again.
            left.push(mountpoint);
        }
    }
    left
}

#[cfg(all(test, feature = "mount"))]
mod tests {
    use super::*;

    /// A held claim is in both registers; a dropped one is in neither.
    #[test]
    fn a_claim_lasts_exactly_as_long_as_it_is_held() {
        let path = std::env::temp_dir().join("cortex-claim-probe");
        let resolved = resolved(&path);

        // Checks this path only: the register is process-wide and tests run in parallel.
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

    /// A live owner's records are never reclaimed, so a second instance cannot unmount the
    /// first's tree.
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

    /// A claim dropped while its mount is still up keeps the record for a later run.
    #[test]
    #[ignore = "mounts a real filesystem"]
    fn a_record_outlives_a_claim_dropped_over_a_live_mount() {
        #[cfg(not(target_os = "macos"))]
        use crate::fs::FuseMount as HostMount;
        #[cfg(target_os = "macos")]
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

        // A second claim on the same point, so dropping it leaves the mount up, as a failed
        // teardown does.
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

    /// A record from a pid that cannot exist is abandoned and cleared.
    #[test]
    fn a_record_from_a_dead_owner_is_cleared() {
        let Some(dir) = registry() else {
            panic!("the registry is usable under $TMPDIR");
        };
        // Above any pid these systems hand out, so it never names a live process.
        let record = dir.join(format!("{}-cortex-test", libc::pid_t::MAX));
        let mountpoint = std::env::temp_dir().join("cortex-claim-dead-probe");
        fs::create_dir_all(&mountpoint).unwrap();
        fs::write(&record, mountpoint.as_os_str().as_bytes()).unwrap();

        reclaim_abandoned();
        assert!(!record.exists(), "an abandoned record is cleared");
        fs::remove_dir_all(&mountpoint).ok();
    }
}
