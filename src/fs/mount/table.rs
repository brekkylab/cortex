//! The host's mount table, and taking a mount down without holding a thread on it.
//!
//! These mounts may not be this process's (a previous run's, or one a guard is releasing),
//! and a mount whose server is gone is still registered with nothing answering, so `stat`ing
//! it hangs. The mount table answers from the kernel's list immediately instead.
//!
//! `unmount(2)` has no timeout, so [`unmount_under`] bounds the wait from outside the process.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// How long one `umount` gets. Past this a person has to deal with the mount, and waiting
/// longer only stalls the sweep.
const UNMOUNT_DEADLINE: Duration = Duration::from_secs(3);

/// How often a child `umount` is checked on while it runs.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Every mount point the operating system has at or under `root`.
///
/// Includes `root` itself when it is a mount point. Never touches the path, since `exists`,
/// `metadata` and the like would hang on a wedged mount.
///
/// `root` is resolved here: the comparison is textual (whole components) and the table spells
/// paths resolved, e.g. macOS `$TMPDIR` `/var/folders/…` appears as `/private/var/folders/…`,
/// so an unresolved root would miss a real mount.
///
/// `getfsstat`, not `getmntinfo`: the latter returns a shared internal buffer, which races
/// when guards tear down concurrently.
#[cfg(target_os = "macos")]
pub(crate) fn mounts_under(root: &Path) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let root = resolved(root);

    // `MNT_NOWAIT` cannot block on a wedged filesystem; only per-fs stats may be stale, and
    // only the mount point is read.
    //
    // SAFETY: a null buffer asks only for the count, which is what the argument
    // pair says.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count <= 0 {
        return Vec::new();
    }
    // Room for a few mounts to appear between the two calls; `getfsstat` writes
    // no more than the buffer allows and reports what it wrote.
    let mut entries: Vec<libc::statfs> = Vec::with_capacity(count as usize + 8);
    let bytes = (entries.capacity() * size_of::<libc::statfs>()) as libc::c_int;
    // SAFETY: `entries` has room for `bytes` worth of `statfs`, and the return
    // value is how many were written.
    let written = unsafe { libc::getfsstat(entries.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if written <= 0 {
        return Vec::new();
    }
    // SAFETY: `getfsstat` initialized exactly this many entries.
    unsafe { entries.set_len(written as usize) };

    entries
        .iter()
        .filter_map(|entry| {
            // SAFETY: `f_mntonname` is a NUL-terminated C string in a fixed array.
            let name = unsafe { std::ffi::CStr::from_ptr(entry.f_mntonname.as_ptr()) };
            // Bytes, since a mount point need not be UTF-8.
            let path = PathBuf::from(OsString::from_vec(name.to_bytes().to_vec()));
            path.starts_with(&root).then_some(path)
        })
        .collect()
}

#[cfg(target_os = "linux")]
pub(crate) fn mounts_under(root: &Path) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let root = resolved(root);
    // Field 5 of each line is the mountpoint, with spaces escaped as `\040`.
    let Ok(table) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    table
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(|p| PathBuf::from(OsString::from_vec(p.replace("\\040", " ").into_bytes())))
        .filter(|p| p.starts_with(&root))
        .collect()
}

/// How a mount is asked to come down, in the order it is asked.
///
/// Plain `umount` first, since it fails safely: a mount in use refuses with `EBUSY`.
///
/// Then force, since dropping the guard already declared the mount over. On macOS that is
/// `diskutil unmount force` (`umount -f` needs root and answers `EPERM`); on Linux a lazy
/// detach, which unlinks now and lets the last reference finish.
#[cfg(target_os = "macos")]
const LADDER: [&[&str]; 2] = [&["umount"], &["diskutil", "unmount", "force"]];

#[cfg(target_os = "linux")]
const LADDER: [&[&str]; 2] = [&["umount"], &["umount", "-l"]];

/// Take down every mount at or under `root`.
///
/// Each attempt runs in a **child process**, the only thing that can be abandoned:
/// `unmount(2)` has no timeout, and a timed-out thread would leak blocked forever. That makes
/// the forceful rung safe too, since `diskutil` can hang and a hung child gets killed.
///
/// `true` when nothing is mounted under `root` any more.
///
/// Success is read off the mount table, never an exit status: `umount` exits 1 both for a
/// mount it could not release and for a path no longer mounted.
///
/// `root` need not be resolved.
pub(crate) fn unmount_under(root: &Path) -> bool {
    for mountpoint in mounts_under(root) {
        for rung in LADDER {
            let (command, leading) = rung.split_first().expect("every rung names a command");
            let spawned = std::process::Command::new(command)
                .args(leading)
                .arg(&mountpoint)
                // Silenced: the first rung's "Resource busy" is expected and would make a
                // successful forced teardown look like a failure. Callers report survivors.
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if let Ok(mut child) = spawned {
                wait_briefly(&mut child);
            }
            // Rechecked so a forceful rung never hits a path that may now be something else.
            if !mounts_under(&mountpoint).iter().any(|m| m == &mountpoint) {
                break;
            }
        }
    }

    mounts_under(root).is_empty()
}

/// Wait for `child` up to [`UNMOUNT_DEADLINE`], then give up on it.
///
/// Killed but **not reaped** on overrun: a child wedged in the kernel on this mount does not
/// act on `SIGKILL` until its syscall returns, so `wait()` could block forever. A zombie is
/// the cheaper leak.
fn wait_briefly(child: &mut std::process::Child) {
    let deadline = Instant::now() + UNMOUNT_DEADLINE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            _ => {
                let _ = child.kill();
                return;
            }
        }
    }
}

/// `path` with its parent resolved, the spelling the mount table uses.
///
/// Never `path` itself: it may be a mount whose server is gone, and `canonicalize` would
/// `stat` it and hang.
///
/// For callers that resolve once and compare many times.
pub(crate) fn resolved(path: &Path) -> PathBuf {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return path.to_path_buf();
    };
    match parent.canonicalize() {
        Ok(parent) => parent.join(name),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The root of the filesystem is always mounted, whatever else is.
    #[test]
    fn the_table_names_the_root_filesystem() {
        assert!(
            mounts_under(Path::new("/"))
                .iter()
                .any(|p| p == Path::new("/")),
            "`/` is mounted on every host this runs on"
        );
    }

    #[test]
    fn a_directory_with_nothing_mounted_under_it_reports_nothing() {
        let dir = std::env::temp_dir().join(format!("cortex-table-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(mounts_under(&resolved(&dir)).is_empty());

        // ...and sweeping it leaves nothing behind.
        assert!(unmount_under(&resolved(&dir)));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An unresolved macOS `$TMPDIR` matches no table row, so resolving must change it.
    #[test]
    #[cfg(target_os = "macos")]
    fn resolving_a_temp_path_changes_it() {
        let raw = std::env::temp_dir().join("cortex-resolve-probe");
        assert!(
            resolved(&raw).starts_with("/private"),
            "macOS `$TMPDIR` resolves under /private; got {}",
            resolved(&raw).display()
        );
    }
}
