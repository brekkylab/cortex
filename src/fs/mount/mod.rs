//! [`Mount`], a tree the operating system has attached, and the bindings that produce one.
//!
//! Each binding translates one filesystem interface onto the layer below
//! ([`Posix`](super::Posix) for interfaces that speak inodes and handles,
//! [`FileSystem`](super::FileSystem) for ones that speak paths) and exports a guard that mounts
//! on construction. The `mount` feature decides whether a build has a binding, the target OS
//! which one. The guards are gated, [`Mount`] is not, so a consumer can take a host-mounted
//! tree without compiling an interface it never mounts through.
//!
//! # Mounts nobody owns any more
//!
//! A guard covers every exit that runs a destructor; a signal runs none, leaving the mount
//! registered with nothing answering it. [`unmount_on_signal`] covers catchable signals and
//! [`reclaim_abandoned`] `SIGKILL`. Neither needs a binding: they concern mounts the host has,
//! and the run cleaning up after a killed process is usually not the one that mounted. Both
//! reuse the guards' own teardown, so a cortex mount comes down one way whether its owner is
//! alive or not. Unix only: a claim is a pid and a mode, and a sweep is `kill(pid, 0)`.

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
