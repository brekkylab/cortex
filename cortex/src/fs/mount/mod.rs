//! [`Mount`] — a tree the operating system has attached — and the bindings that produce one.
//!
//! The split is between what a mount *is* and what makes one. [`Mount`] is the first, and is
//! all a consumer needs: a mounted tree is a path, and every binding's guard reports one the
//! same way. The bindings are the second, one per concrete filesystem interface, each
//! translating that interface's calls onto the layer below — [`Posix`](super::Posix) where the
//! interface speaks inodes and handles, [`FileSystem`](super::FileSystem) directly where it
//! speaks paths — and exporting a guard that mounts on construction.
//!
//! Which bindings a build has is a feature, and the split is why that stays contained: the
//! guards are re-exported gated, the trait is not, so a consumer can take a tree the host has
//! mounted without compiling an interface it will never mount through.
//!
//! # Mounts nobody owns any more
//!
//! A guard covers every exit that runs a destructor, which is every exit but one: a signal
//! stops the process between two instructions and the mount stays registered with nothing
//! answering it. There is nothing for a guard to do about that, so the two things that can
//! sit beside it instead:
//!
//! * [`reclaim_abandoned`] — for `SIGKILL`, where the only run that can help is the next one.
//!   Every `try_new` calls it, so a consumer gets this without asking; it takes down only what
//!   a *dead* process owned, which is what makes doing it unasked safe.
//! * [`unmount_on_signal`] — opt-in, for the signals that can be caught. It does not change
//!   whether an abandoned mount is cleared, only whether it waits for the next run.
//!
//! Taking a mount off the host's table is the mechanism under both, and stays inside: a
//! consumer names *what it owns* — a guard, or the register — never a path to sweep, because
//! a path says nothing about whose mount is on it.
//!
//! Neither is gated on a binding, and both are what a *consumer* of the trait needs rather
//! than what a binding needs: clearing a mount a dead run left takes no binding at all. Both
//! are also built out of what the guards' own teardown uses, so a cortex mount comes down
//! exactly one way whether its owner is alive or not. Unix only, because a claim is a pid and
//! a mode and a sweep is `kill(pid, 0)`.

#[cfg(unix)]
mod claim;
mod r#impl;
mod mount;
#[cfg(unix)]
mod signal;
#[cfg(unix)]
mod table;

// The guards themselves are each gated on the binding that exports them, so a build without
// that feature has no name for one — which is the point: it cannot mount that way either.
// Not gated on a binding, like the trait and for the same reason: recovering a mount point
// and taking this process's mounts down on a signal are about mounts the *host* has, which
// outlives which binding made one — and the run that has to clean up after a killed process
// is usually not the run that mounted.
#[cfg(unix)]
pub use claim::reclaim_abandoned;
#[cfg(all(feature = "dokan", windows))]
pub use r#impl::{DokanMount, MountFlags};
#[cfg(feature = "fuse")]
pub use r#impl::{FuseMount, MountOption};
#[cfg(feature = "fuse-t")]
pub use r#impl::{FuseTBackend, FuseTMount};
pub use mount::*;
#[cfg(unix)]
pub use signal::unmount_on_signal;
