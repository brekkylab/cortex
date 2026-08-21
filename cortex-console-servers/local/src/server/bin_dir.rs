//! The per-process scratch directory: where virtual executables become real,
//! and where a workspace is mounted.
//!
//! An executable is virtual only in that its behaviour lives in our code rather
//! than in a file. `execvp` does not care where behaviour lives, but it does
//! insist on finding a name on `PATH` backed by something the kernel can exec.
//! So we give it exactly that: a directory of symlinks, one per registered
//! name, every one of them pointing back at this binary.
//!
//! The mount point is a sibling of that directory rather than one of its own,
//! so a single naming convention and a single sweep cover both.
//!
//! One binary, N names. The link is what `execvp` resolves; `argv[0]` is what
//! tells the re-executed process which name it was called by. Nothing is built
//! per executable and nothing is shipped alongside — adding one costs a
//! `symlink(2)`.
//!
//! The directory is named for our pid under `$TMPDIR` and deleted on `Drop`.
//! `Drop` cannot run when we are killed outright, so [`sweep_stale`] collects
//! what earlier runs left behind: the pid in the name says whose it was, and a
//! signal-0 probe says whether that process is still around.

use std::{
    fs::{self, DirBuilder},
    io,
    os::unix::fs::{DirBuilderExt, symlink},
    path::{Path, PathBuf},
    time::Duration,
};

/// Marks a directory as ours and carries the owning pid, so a later run can
/// tell an abandoned directory from one still in use.
const PREFIX: &str = "cortex-console-";

/// Our scratch directory: the `bin/` that goes on `PATH`, and the `mnt/` a
/// workspace is mounted on.
///
/// The socket a shim dials is not here. It is bound once for the process rather than per
/// session, so its lifetime is not this directory's — see
/// [`Shims`](super::Shims) and [`ipc`](crate::ipc).
///
/// Owns the directory outright — dropping it removes the tree, so the caller
/// keeps it alive for exactly as long as the names should be callable.
pub struct SessionScratch {
    root: PathBuf,
}

impl SessionScratch {
    /// Create the directory and link every name in `names` to this binary.
    ///
    /// Old directories are swept first: a run that was killed cannot have
    /// cleaned up after itself, and nobody else will.
    pub fn create<'a>(names: impl IntoIterator<Item = &'a str>) -> io::Result<Self> {
        // Resolved, so that every path derived from this one is the path the *kernel*
        // would report. `$TMPDIR` on macOS is `/var/folders/…` and `/var` is a symlink to
        // `private/var`, so a mount point built from the raw value differs textually from
        // what `getcwd` hands a process standing in it and from what the mount table says.
        // Everything downstream compares those textually — `relativize` is a
        // `strip_prefix`, `mounts_under` a `starts_with` — so resolving here is what makes
        // one directory have one name.
        //
        // The fallback keeps a session working rather than refusing to boot over a
        // cosmetic path, but it is the one state in which every comparison below silently
        // goes back to being wrong — so it says so. `$TMPDIR` failing to resolve takes
        // something extraordinary; nobody should have to guess that it happened.
        let tmp = std::env::temp_dir();
        let tmp = tmp.canonicalize().unwrap_or_else(|e| {
            eprintln!(
                "{}: {} will not resolve ({e}) — paths under it may not match what the \
                 kernel reports, and a stale mount may not be found",
                env!("CARGO_BIN_NAME"),
                tmp.display()
            );
            tmp.clone()
        });
        sweep_stale(&tmp);

        let root = tmp.join(format!("{PREFIX}{}", std::process::id()));
        // A leftover from a *live* pid is impossible (pids are unique) and one
        // from a dead pid was just swept, so anything here is a reused pid whose
        // directory outlived the sweep. Start clean either way.
        let _ = fs::remove_dir_all(&root);

        // 0700: every name in here re-enters this binary as a shim, so the directory
        // permission is what keeps other users from putting one on our `PATH`.
        DirBuilder::new().mode(0o700).create(&root)?;
        let dir = SessionScratch { root };
        DirBuilder::new().mode(0o700).create(dir.bin())?;
        DirBuilder::new().mode(0o700).create(dir.mnt())?;

        // Resolved, not `argv[0]`: the link must point at the binary itself, or
        // it would break the moment the caller's cwd changed.
        let exe = std::env::current_exe()?;
        for name in names {
            symlink(&exe, dir.bin().join(name))?;
        }
        Ok(dir)
    }

    /// The directory to put on `PATH`.
    pub fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// Where a workspace is mounted, when there is one to mount.
    ///
    /// A sibling of [`bin`](Self::bin) rather than a directory of its own, so one naming
    /// convention and one sweep cover both. It is only ever a *place*: a mount covers it,
    /// and nothing reads what is underneath.
    pub fn mnt(&self) -> PathBuf {
        self.root.join("mnt")
    }
}

impl Drop for SessionScratch {
    fn drop(&mut self) {
        // Nothing useful to do about a failure here: we are on the way out, and
        // the next run's sweep will catch whatever is left.
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// How long the sweep will wait on one `umount` before giving up on that root.
///
/// This is time the whole connection is blocked — [`sweep_stale`] runs inside
/// [`SessionScratch::create`], which the server calls from its single request
/// loop — so it is short. A mount that does not come back in this long is one a
/// person will have to deal with, and waiting longer only makes the session pay
/// for it too.
const UNMOUNT_DEADLINE: Duration = Duration::from_secs(3);

/// How many stale roots one boot will spend an unmount on.
///
/// Each costs up to [`UNMOUNT_DEADLINE`] on that same request loop, so an
/// unbounded scan makes a host's accumulated wreckage everybody's latency.
/// Whatever is left over is the next boot's to try.
///
/// Only unmounts are charged. A stale root with nothing mounted under it costs a
/// `remove_dir_all` and no wait at all, so charging it would slow the common case
/// down to defend against a cost it does not have.
const SWEEP_BUDGET: usize = 4;

/// Every mountpoint the OS has under `root`.
///
/// The mount table and never the filesystem: a dead process's mount is still
/// registered and nothing answers it, so `stat`ing the path to look is exactly
/// the hang this exists to avoid.
#[cfg(target_os = "macos")]
fn mounts_under(root: &Path) -> Vec<PathBuf> {
    let mut buf: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: `getmntinfo` fills `buf` with a pointer to a static array it owns
    // and returns its length; we only read it, and only before any other call.
    let n = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
    if n <= 0 {
        return Vec::new();
    }
    // SAFETY: `n` entries were just reported at `buf`.
    let entries = unsafe { std::slice::from_raw_parts(buf, n as usize) };
    entries
        .iter()
        .filter_map(|e| {
            // SAFETY: `f_mntonname` is a NUL-terminated C string in a fixed array.
            let name = unsafe { std::ffi::CStr::from_ptr(e.f_mntonname.as_ptr()) };
            let path = PathBuf::from(name.to_str().ok()?);
            path.starts_with(root).then_some(path)
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn mounts_under(root: &Path) -> Vec<PathBuf> {
    // Field 5 of each line is the mountpoint, with spaces escaped as `\040`.
    let Ok(table) = fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    table
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(|p| PathBuf::from(p.replace("\\040", " ")))
        .filter(|p| p.starts_with(root))
        .collect()
}

/// Give back a dead session's mounts, or give up on that root.
///
/// `false` means a mount under it survived, so the directory has to stay:
/// removing it would block, and a leaked directory is recoverable by a person
/// where a wedged request loop is not. Whether one survived is asked of the
/// mount table afterwards rather than of `umount`'s exit status — see below.
///
/// The unmount runs in a **child process** because that is the only shape that
/// can be given up on. `libc::unmount` is a syscall with no timeout, and a
/// timeout around a worker thread would bound only how long this waits — the
/// thread itself would be leaked, blocked forever, one more per stale root the
/// sweep ever meets.
///
/// `charged` reports whether this root actually cost a wait, which is what
/// [`SWEEP_BUDGET`] bounds; a root with nothing mounted under it spends nothing.
fn unmount_stale(root: &Path, charged: &mut bool) -> bool {
    // Resolved first, because the kernel's answer is resolved and `starts_with`
    // is not. `std::env::temp_dir()` on macOS is `/var/folders/…`, `/var` is a
    // symlink to `private/var`, and the mount table says `/private/var/folders/…`.
    // `Path::starts_with` compares whole components and follows nothing, so an
    // unresolved root matches no row and `mounts_under` comes back empty for
    // *every* stale root — the sweep would skip to `remove_dir_all` and block,
    // which is the entire bug this function exists to prevent.
    //
    // Resolving the root is safe where resolving a table row is not: the root is
    // an ordinary directory and the mount is at `mnt/` *under* it, so this walk
    // never enters the wedged path.
    let root = match root.canonicalize() {
        Ok(root) => root,
        // Gone already — somebody else's sweep, or a session that cleaned up
        // after this scan began. Nothing to unmount, and the caller's
        // `remove_dir_all` is a no-op on a path that is not there.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            *charged = false;
            return true;
        }
        // Anything else — a permission race, a broken link partway down — means
        // we cannot ask what is mounted under it. Not knowing is not the same as
        // nothing being there, and answering `true` would send the caller into
        // `remove_dir_all` on a root that may still hold an unanswered mount:
        // the exact hang. Leave it for a later boot, and say so, since this is
        // the one branch that reaches neither the mount table nor the log below.
        Err(e) => {
            eprintln!(
                "{}: leaving {} — cannot resolve it to ask what is mounted under it: {e}",
                env!("CARGO_BIN_NAME"),
                root.display()
            );
            *charged = false;
            return false;
        }
    };

    let mounts = mounts_under(&root);
    if std::env::var_os("CORTEX_SWEEP_DEBUG").is_some() {
        eprintln!(
            "{}: sweep {} -> {mounts:?}",
            env!("CARGO_BIN_NAME"),
            root.display()
        );
    }
    *charged = !mounts.is_empty();

    for mountpoint in mounts {
        // Plain `umount`. `-f` needs root and answers EPERM; `diskutil unmount
        // force` hangs. Both established by hand on a wedged mount.
        let Ok(mut child) = std::process::Command::new("umount")
            .arg(&mountpoint)
            .spawn()
        else {
            continue;
        };

        let deadline = std::time::Instant::now() + UNMOUNT_DEADLINE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                // Overran, or the wait itself failed. Signalled but **not
                // reaped**: `kill` only queues SIGKILL, and a child wedged in the
                // kernel on this very mount does not act on it until its syscall
                // returns — which is the case this mechanism exists to survive.
                // A `wait()` here would block forever and put the hang back one
                // layer down, in the request loop. A zombie is the cheaper leak,
                // and it is the same trade as leaving the directory.
                _ => {
                    let _ = child.kill();
                    break;
                }
            }
        }
    }

    // What survived is the mount table's answer and not `umount`'s exit status, which cannot
    // tell the two cases apart: it is 1 both for a mount it could not release and for a path
    // that was never mounted — measured, `umount` on a plain directory prints "not currently
    // mounted" and exits 1.
    //
    // The two disagree by design. `getmntinfo` is asked with `MNT_NOWAIT` so that it cannot
    // block on a wedged filesystem, which means it answers from a cache that may still name a
    // mount already gone. Reading a failed `umount` as "it survived" therefore costs a phantom
    // row three things: the root stays another boot, a [`SWEEP_BUDGET`] slot is spent where a
    // genuinely wedged root behind it goes untouched, and the log says a mount is stuck when
    // nothing is there.
    let survivors = mounts_under(&root);
    for mountpoint in &survivors {
        eprintln!(
            "{}: leaving {} — its mount did not come back",
            env!("CARGO_BIN_NAME"),
            mountpoint.display()
        );
    }
    survivors.is_empty()
}

/// Remove scratch directories whose owning process is gone.
///
/// `kill(pid, 0)` sends no signal and only reports whether the pid can be
/// signalled. `ESRCH` — no such process — is the one answer that means the
/// directory is abandoned; `EPERM` says the pid is alive under another user, so
/// the directory is left alone.
///
/// An abandoned root is unmounted before it is removed. Once a session mounts a
/// workspace at `mnt/`, a root left by a killed process still has that mount
/// registered with nothing answering it, and `remove_dir_all` on it blocks.
fn sweep_stale(tmp: &Path) {
    let Ok(entries) = fs::read_dir(tmp) else {
        return;
    };
    let mut budget = SWEEP_BUDGET;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(PREFIX))
            .and_then(|p| p.parse::<i32>().ok())
        else {
            continue;
        };
        // SAFETY: `kill` with signal 0 performs only the permission/existence
        // check; it cannot affect this or any other process.
        let gone = unsafe { libc::kill(pid, 0) } == -1
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if !gone {
            continue;
        }
        if budget == 0 {
            // The rest is the next boot's. Better than making this request wait
            // on a backlog somebody else's crashes left.
            break;
        }

        let mut charged = false;
        if unmount_stale(&entry.path(), &mut charged) {
            let _ = fs::remove_dir_all(entry.path());
        }
        // Charged after the fact, and only if there was something to wait on: a
        // root that had no mount cost what it always did, and does not eat a turn
        // somebody else's wreckage needs.
        if charged {
            budget -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_has_a_bin_and_a_mount_point_under_one_root() {
        let scratch = SessionScratch::create(["foo"]).unwrap();

        assert!(scratch.bin().is_dir(), "bin/ exists for PATH");
        assert!(scratch.mnt().is_dir(), "mnt/ exists for a mount to land on");
        assert_eq!(
            scratch.bin().parent(),
            scratch.mnt().parent(),
            "one root, so one sweep covers both"
        );
        assert!(
            std::fs::read_dir(scratch.mnt()).unwrap().next().is_none(),
            "a mount point starts empty — a mount covers it, it holds nothing itself"
        );
    }
}
