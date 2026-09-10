//! [`Mount`] — a tree the operating system has attached — and the bindings that produce one.
//!
//! The split is between what a mount *is* and what makes one. [`Mount`] is the first, and is
//! all a consumer needs: a mounted tree is a path, and every binding's guard reports one the
//! same way. The bindings are the second, one per concrete filesystem interface, each
//! translating that interface's calls onto [`Posix`](super::Posix) and exporting a guard that
//! mounts on construction.
//!
//! Which bindings a build has is a feature, and the split is why that stays contained: the
//! guards are re-exported gated, the trait is not, so a consumer can take a tree the host has
//! mounted without compiling an interface it will never mount through.
//!
//! # Mounts nobody owns any more
//!
//! A guard covers every exit that runs a destructor, which is every exit but one: a signal
//! stops the process between two instructions and the mount stays registered with nothing
//! answering it. There is nothing for a guard to do about that, so what sits beside it is
//! [`unmount_on_signal`] — opt-in, for the signals that can be caught.
//!
//! `SIGKILL` reaches no handler at all, and no care taken here can change that: a mount it
//! leaves is cleared by whoever comes next, which the commit after this one gives that run a
//! safe way to be.
//!
//! Taking a mount off the host's table is the mechanism under both a guard's teardown and
//! that handler, and stays inside the crate: a consumer names *what it owns* — a guard —
//! never a path to sweep, because a path says nothing about whose mount is on it.
//!
//! `unmount_on_signal` is ungated, like the trait and for the same reason: what it takes down
//! is a mount the *host* has, and clearing one needs no binding compiled in.

mod r#impl;
mod mount;
mod signal;
mod table;

// The guards themselves are each gated on the binding that exports them, so a build without
// that feature has no name for one — which is the point: it cannot mount that way either.
#[cfg(feature = "fuse")]
pub use r#impl::{FuseMount, MountOption};
#[cfg(feature = "fuse-t")]
pub use r#impl::{FuseTBackend, FuseTMount};
pub use mount::*;
// Ungated, like the trait and for the same reason: recovering a mount point and
// taking this process's mounts down on a signal are about mounts the *host* has,
// which outlives which binding made one — and the run that has to clean up after
// a killed process is usually not the run that mounted.
pub use signal::unmount_on_signal;
