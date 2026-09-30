//! The host's mount table, and taking a mount down without holding a thread on it.
//!
//! Everything here is about mounts *this process may not own* — the ones a
//! previous run left behind, and the ones a guard is about to release. Neither
//! can be asked about through the filesystem: a mount whose server is gone is
//! still registered, and nothing answers it, so `stat`ing the path to see
//! whether it is there is the exact hang this exists to avoid. The mount table
//! answers from the kernel's own list instead, and answers immediately.
//!
//! [`unmount_under`] is the other half. `unmount(2)` is a syscall with no
//! timeout, so the wait has to be bounded from outside the process that makes
//! it — see the comments there.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// How long one `umount` gets before it is given up on.
///
/// A mount that does not come back in this long is one a person will have to
/// deal with, and waiting longer only makes whoever is sweeping pay for it too.
const UNMOUNT_DEADLINE: Duration = Duration::from_secs(3);

/// How often a child `umount` is checked on while it runs.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Every mount point the operating system has at or under `root`.
///
/// Asked of the mount table and never of the filesystem, for the reason in this
/// module's docs — which is also what makes this the only way to ask whether
/// something is mounted without risking the hang: `exists`, `metadata` and every
/// other `std` answer goes through the path itself.
///
/// `root` is taken as given and resolved here. The comparison against the table
/// is textual (`starts_with`, whole components) and the table spells paths the
/// long way — on macOS `$TMPDIR` is `/var/folders/…`, `/var` is a symlink to
/// `private/var`, and the table says `/private/var/folders/…` — so an unresolved
/// root would match no row and answer "nothing is mounted" about a mount that is.
/// Resolving is safe to do to a mount point precisely because only the *parent* is
/// resolved — never the path itself, which may be the wedged one — so doing it here
/// costs the caller nothing to get right.
///
/// Includes `root` itself when `root` is the mount point, which is the common
/// case for a single guard.
///
/// `getfsstat` and not `getmntinfo`, which is the obvious call and the wrong one:
/// it hands back a pointer to a buffer it owns and reuses, so two threads asking
/// at once get each other's answer or a half-written one. Every guard's teardown
/// polls this, so "two at once" is the normal case here rather than an unusual
/// one. `getfsstat` fills a buffer the caller owns.
#[cfg(target_os = "macos")]
pub(crate) fn mounts_under(root: &Path) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let root = resolved(root);

    // `MNT_NOWAIT` so that it cannot block on a wedged filesystem — which means
    // the per-filesystem numbers may be stale, though the list of mounts itself
    // is current. Nothing here reads anything but the mount point.
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
            // Bytes rather than `to_str`: a mount point is a path and need not
            // be UTF-8, and one that is not is still a path that has to come
            // down.
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
/// Plain `umount` first, because it is the one that fails safely: it refuses a
/// mount that is still in use (`EBUSY`, "Resource busy") rather than pulling it
/// out from under whoever is using it. A mount with readers on it refuses every
/// time, which is why the first rung is never the whole answer.
///
/// Then force, because a caller that has dropped its guard has already said the
/// mount is over, and a mount nobody can take down is the failure this whole
/// module exists to avoid. On macOS that is `diskutil`, which is the tool that
/// works: `umount -f` needs root and answers `EPERM`, while `diskutil unmount
/// force` took down a busy mount that plain `umount` had just refused. On Linux
/// it is a lazy detach, which unlinks the mount now and lets the last reference
/// finish on its own.
#[cfg(target_os = "macos")]
const LADDER: [&[&str]; 2] = [&["umount"], &["diskutil", "unmount", "force"]];

#[cfg(target_os = "linux")]
const LADDER: [&[&str]; 2] = [&["umount"], &["umount", "-l"]];

/// Take down every mount at or under `root`.
///
/// Each attempt runs in a **child process**, because that is the only shape that
/// can be given up on: `libc::unmount` is a syscall with no timeout, and a
/// timeout around a worker thread would bound only how long this waits — the
/// thread itself would be leaked, blocked forever, one more per wedged mount this
/// ever meets. Every rung of the ladder is bounded that way, which is what makes
/// trying a forceful one safe: `diskutil` has been seen to hang, and a hang in a
/// child is a child that gets killed.
///
/// `true` when nothing is mounted under `root` any more. Whoever wants to say *what*
/// survived already knows the path it asked about.
///
/// Whether a mount came down is read off the mount table between rungs, never
/// from an exit status, which cannot tell the two cases apart: `umount` exits 1
/// both for a mount it could not release and for a path that was never mounted.
/// Since [`mounts_under`] may name a mount already gone, reading a failed
/// `umount` as "it survived" would report a phantom every time.
///
/// `root` is taken as given: [`mounts_under`] resolves it, so a caller that holds
/// `$TMPDIR/…` hands that over and does not have to know how the table spells it.
pub(crate) fn unmount_under(root: &Path) -> bool {
    for mountpoint in mounts_under(root) {
        for rung in LADDER {
            let (command, leading) = rung.split_first().expect("every rung names a command");
            let spawned = std::process::Command::new(command)
                .args(leading)
                .arg(&mountpoint)
                // Silenced, because the first rung refusing a busy mount is the
                // expected path to the second one — printing its "Resource busy"
                // would make every successful forceful teardown look like a
                // failure. What survived is reported by the caller instead.
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if let Ok(mut child) = spawned {
                wait_briefly(&mut child);
            }
            // Asked again before the next rung, so a mount that came down does
            // not get a forceful one aimed at it — by which time the path may
            // belong to something else entirely.
            if !mounts_under(&mountpoint).iter().any(|m| m == &mountpoint) {
                break;
            }
        }
    }

    mounts_under(root).is_empty()
}

/// Wait for `child` up to [`UNMOUNT_DEADLINE`], then give up on it.
///
/// Signalled but **not reaped** on overrun: `kill` only queues `SIGKILL`, and a
/// child wedged in the kernel on this very mount does not act on it until its
/// syscall returns — which is the case this bounding exists to survive. A
/// `wait()` here would block forever and put the hang back one layer down. A
/// zombie is the cheaper leak.
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

/// `path` with its parent resolved — the spelling the mount table uses.
///
/// Only the parent is resolved, never `path` itself: the parent is an ordinary
/// directory, where `path` may be a mount point whose server is gone, and
/// `canonicalize` on that is a `stat` on the wedged path.
///
/// Not public, because nothing outside has to think about it: [`mounts_under`]
/// and [`unmount_under`] resolve what they are given, and a guard resolves its own
/// mount point once when it claims it. What is left here is the one caller that
/// wants to resolve *once* and compare many times.
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
        let dir = std::env::temp_dir().join(format!("virtx-table-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(mounts_under(&resolved(&dir)).is_empty());

        // ...and sweeping it leaves nothing behind.
        assert!(unmount_under(&resolved(&dir)));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An unresolved `$TMPDIR` matches no row on macOS, which is the failure
    /// `resolved` exists to prevent — so the resolved form has to differ from
    /// the raw one, or the guard is not doing anything.
    #[test]
    #[cfg(target_os = "macos")]
    fn resolving_a_temp_path_changes_it() {
        let raw = std::env::temp_dir().join("virtx-resolve-probe");
        assert!(
            resolved(&raw).starts_with("/private"),
            "macOS `$TMPDIR` resolves under /private; got {}",
            resolved(&raw).display()
        );
    }
}
