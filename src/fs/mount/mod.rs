//! [`Mount`], a tree the operating system has attached, and the bindings that produce one.
//!
//! [`Mount`] is all a consumer needs: a mounted tree is a path. Each binding translates one
//! filesystem interface onto the layer below ([`Posix`](super::Posix) for interfaces that speak
//! inodes and handles, [`FileSystem`](super::FileSystem) for ones that speak paths) and exports
//! a guard that mounts on construction.
//!
//! The `mount` feature decides whether a build has a binding, the target OS which one. Only
//! the guards are gated, so a consumer can take a host-mounted tree without compiling an
//! interface it never mounts through.
//!
//! # Mounts nobody owns any more
//!
//! A guard covers every exit that runs a destructor. A signal runs none, leaving the mount
//! registered with nothing answering it. Two things cover that:
//!
//! * [`reclaim_abandoned`], for `SIGKILL`, where only the next run can help. Every `try_new`
//!   calls it; it takes down only what a *dead* process owned, which is what makes that safe.
//! * [`unmount_on_signal`], opt-in, for catchable signals. It only changes whether an
//!   abandoned mount waits for the next run.
//!
//! A consumer names what it owns (a guard, or the register), never a path to sweep, because a
//! path says nothing about whose mount is on it. Neither needs a binding, and both reuse the
//! guards' own teardown, so a cortex mount comes down one way whether its owner is alive or
//! not. Unix only: a claim is a pid and a mode, and a sweep is `kill(pid, 0)`.

#[cfg(unix)]
mod claim;
mod r#impl;
mod mount;
#[cfg(unix)]
mod signal;
#[cfg(all(feature = "mount", any(unix, windows)))]
mod support;
#[cfg(unix)]
mod table;

// Guards are gated on their binding; recovery and signal teardown are not, since they concern
// mounts the host has, and the run cleaning up after a killed process is usually not the one
// that mounted.
#[cfg(unix)]
pub use claim::reclaim_abandoned;
#[cfg(all(feature = "mount", windows))]
pub use r#impl::{DokanMount, MountFlags};
#[cfg(all(feature = "mount", unix, not(target_os = "macos")))]
pub use r#impl::{FuseMount, MountOption};
#[cfg(all(feature = "mount", target_os = "macos"))]
pub use r#impl::{FuseTBackend, FuseTMount};
pub use mount::*;
#[cfg(unix)]
pub use signal::unmount_on_signal;
#[cfg(all(feature = "mount", any(unix, windows)))]
pub use support::mount_support;
