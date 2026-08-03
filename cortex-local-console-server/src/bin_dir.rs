//! The per-process scratch directory that makes virtual executables real.
//!
//! An executable is virtual only in that its behaviour lives in our code rather
//! than in a file. `execvp` does not care where behaviour lives, but it does
//! insist on finding a name on `PATH` backed by something the kernel can exec.
//! So we give it exactly that: a directory of symlinks, one per registered
//! name, every one of them pointing back at this binary.
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

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};

/// Marks a directory as ours and carries the owning pid, so a later run can
/// tell an abandoned directory from one still in use.
const PREFIX: &str = "cortex-console-";

/// `sockaddr_un.sun_path` on macOS. A path that does not fit cannot be bound or
/// connected to, and the failure surfaces far from the cause — so we check it
/// where the path is built rather than where it is used.
const SUN_PATH_MAX: usize = 104;

/// Our scratch directory: the `bin/` on `PATH` plus the socket its shims dial.
///
/// Owns the directory outright — dropping it removes the tree, so the caller
/// keeps it alive for exactly as long as the names should be callable.
pub struct BinDir {
    root: PathBuf,
}

impl BinDir {
    /// Create the directory and link every name in `names` to this binary.
    ///
    /// Old directories are swept first: a run that was killed cannot have
    /// cleaned up after itself, and nobody else will.
    pub fn create<'a>(names: impl IntoIterator<Item = &'a str>) -> io::Result<Self> {
        let tmp = std::env::temp_dir();
        sweep_stale(&tmp);

        let root = tmp.join(format!("{PREFIX}{}", std::process::id()));
        // A leftover from a *live* pid is impossible (pids are unique) and one
        // from a dead pid was just swept, so anything here is a reused pid whose
        // directory outlived the sweep. Start clean either way.
        let _ = fs::remove_dir_all(&root);

        // 0700: the socket inside is an unauthenticated channel to `exec`, so
        // the directory permission is what keeps other users off it.
        DirBuilder::new().mode(0o700).create(&root)?;
        let dir = BinDir { root };
        DirBuilder::new().mode(0o700).create(dir.bin())?;

        let sock = dir.socket();
        if sock.as_os_str().len() >= SUN_PATH_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("socket path too long for sun_path: {}", sock.display()),
            ));
        }

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

    /// The socket shims connect back on.
    pub fn socket(&self) -> PathBuf {
        self.root.join("sock")
    }
}

impl Drop for BinDir {
    fn drop(&mut self) {
        // Nothing useful to do about a failure here: we are on the way out, and
        // the next run's sweep will catch whatever is left.
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Remove scratch directories whose owning process is gone.
///
/// `kill(pid, 0)` sends no signal and only reports whether the pid can be
/// signalled. `ESRCH` — no such process — is the one answer that means the
/// directory is abandoned; `EPERM` says the pid is alive under another user, so
/// the directory is left alone.
fn sweep_stale(tmp: &Path) {
    let Ok(entries) = fs::read_dir(tmp) else {
        return;
    };
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
        if gone {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}
