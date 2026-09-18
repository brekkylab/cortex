//! What a mount must not take with it: the process's `SIGCHLD` handling.
//!
//! libfuse starts its mount helper itself, with `fork` and `exec`, and prepares
//! for that by putting `SIGCHLD` back to its default. In libfuse-t 1.2.7 that is
//! the first thing `fuse_mount_core` does, unconditionally:
//!
//! ```text
//! _fuse_mount_core:
//!     …
//!     mov  w0, #0x14        ; SIGCHLD
//!     mov  x1, #0x0         ; SIG_DFL
//!     bl   _signal
//! ```
//!
//! A signal disposition is process-global, so what that resets is not libfuse's
//! `SIGCHLD` but the *program's*. Whatever handler was installed is gone, and
//! nothing says so.
//!
//! # Why this is worse than it sounds
//!
//! `SIGCHLD` is how a process learns its children have exited, and an async
//! runtime's `wait` is built on it: Tokio installs a handler on the first child it
//! spawns, and from then on every `Child::wait` is answered by that handler and
//! nothing else. So a mount that happens *after* the first spawn does not break
//! the mount. It breaks every `wait` the program makes from then on, in code that
//! never heard of a filesystem, with the child having already run to completion
//! and exited cleanly. What a caller sees is a command that did its work and then
//! timed out.
//!
//! It is also order-dependent, which is what makes it look intermittent: a program
//! that mounts once at startup mounts before Tokio has registered anything and
//! never sees it, and the same program remounting mid-run breaks from that point
//! on. `tests/mount_sigchld.rs` is both halves of that.
//!
//! # Restoring rather than preventing
//!
//! There is no way to stop the call from happening: it is inside the library, on
//! the path that mounts. So the disposition is read before and put back after,
//! which is what [`Sigchld`] is.
//!
//! It is put back **only when the call left no handler behind**. Something else in
//! the process may install one while a mount is in progress, and that one is newer
//! than what was read here; overwriting it would be this module committing the
//! same offence it exists to undo. A disposition of `SIG_DFL` or `SIG_IGN` is
//! nobody's handler, so restoring over one takes nothing away.
//!
//! # What it cannot promise
//!
//! The window is not closed, only shut afterwards. A `SIGCHLD` delivered *during*
//! the mount arrives at `SIG_DFL` and is not delivered again, so a child that
//! exits in those seconds goes unnoticed until something else wakes the wait. The
//! only complete answer is the one a caller owns: mount before spawning children,
//! or wait on them without depending on the notification.

/// The process's `SIGCHLD` disposition, held across a call that does not know it
/// is borrowing it, and put back if the call dropped it.
///
/// A guard rather than a pair of calls because the restore has to happen on every
/// way out of the mount, `?` and panic included, and because what it holds is
/// process-global: the shorter the value's life, the smaller the window in which
/// the process is running on someone else's idea of `SIGCHLD`.
pub(crate) struct Sigchld(Option<libc::sigaction>);

impl Sigchld {
    /// Read what the process does with `SIGCHLD` now, to put back on drop.
    pub(crate) fn held() -> Self {
        Self(disposition())
    }
}

impl Drop for Sigchld {
    fn drop(&mut self) {
        let (Some(was), Some(now)) = (self.0, disposition()) else {
            return;
        };
        // Unchanged, which is every mount on a platform where libfuse is not in
        // the path at all: on Linux `fuser` talks to `/dev/fuse` itself and forks
        // nothing.
        if now.sa_sigaction == was.sa_sigaction && now.sa_flags == was.sa_flags {
            return;
        }
        // Someone else's handler, installed while the mount was running. Newer
        // than what this read, so it stays.
        if now.sa_sigaction != libc::SIG_DFL && now.sa_sigaction != libc::SIG_IGN {
            return;
        }
        // SAFETY: `was` is the value `sigaction` itself wrote when this guard was
        // taken, so it is a disposition this process had.
        unsafe { libc::sigaction(libc::SIGCHLD, &was, std::ptr::null_mut()) };
    }
}

/// What the process currently does with `SIGCHLD`, or `None` if it would not say.
///
/// A null `act` asks without setting, which is the only way to read one. The read
/// fails only for a signal number that does not exist, which `SIGCHLD` is not, so
/// `None` here is a case that cannot arise rather than one with a handling: it is
/// `None` so that a guard which learned nothing restores nothing, which is the
/// safe way to be wrong.
fn disposition() -> Option<libc::sigaction> {
    // SAFETY: `current` is written by `sigaction` before it is read, and a null
    // `act` installs nothing.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        (libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut current) == 0).then_some(current)
    }
}
