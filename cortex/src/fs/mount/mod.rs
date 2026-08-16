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

mod r#impl;
mod mount;

pub use mount::*;
// The guards themselves are each gated on the binding that exports them, so a build without
// that feature has no name for one — which is the point: it cannot mount that way either.
#[cfg(feature = "fuse")]
pub use r#impl::{FuseMount, MountOption};
#[cfg(feature = "fuse-t")]
pub use r#impl::{FuseTBackend, FuseTMount};
