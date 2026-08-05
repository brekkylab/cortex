//! One module per concrete filesystem interface, each binding
//! [`PosixFs`](super::PosixFs) to that interface.
//!
//! A binding only translates: it decodes the interface's arguments, calls one
//! of the shared operations, and encodes the reply. The two things a binding
//! decides for itself are the errno numbering its consumer expects (a guest
//! kernel is always Linux; the host's is the host's) and the concrete attribute
//! type it must fill.
//!
//! A binding is a trait impl, so declaring the module is what pulls it in. What
//! is re-exported is each binding's *call surface* — the type a program holds to
//! put the filesystem in front of that interface. The krun binding needs none:
//! its call surface belongs to `msb_krun` (`VmBuilder::fs(..).custom(..)`).

#[cfg(feature = "fuse")]
mod fuse;
#[cfg(feature = "fuse-t")]
mod fuse_t;
// Gated like the others even though it exports nothing. A binding is a trait
// impl, so the module declaration is the only thing pulling it in — and an
// ungated one makes its foreign crate mandatory for every consumer, whether or
// not they ever boot a VM.
#[cfg(feature = "krun")]
mod krun;

#[cfg(feature = "fuse")]
pub use fuse::CortexMount;
#[cfg(feature = "fuse-t")]
pub use fuse_t::FuseTMount;
// Re-exported so a caller can name mount options without taking a direct
// dependency on `fuser`, which is our implementation detail.
#[cfg(feature = "fuse")]
pub use fuser::MountOption;

/// Whichever host binding this build has: `CortexMount` under `fuse`,
/// `FuseTMount` under `fuse-t`.
///
/// Exported because a consumer cannot express it. Features are a crate-level
/// concept, so "whichever of the two is enabled" has no spelling outside the
/// crate that defines them — every consumer would otherwise repeat this pair of
/// `cfg`s, as `tests/host_mount.rs` and `examples/mount_host.rs` both did.
///
/// FUSE-T wins a tie, being the one that needs no kernel extension.
#[cfg(all(feature = "fuse", not(feature = "fuse-t")))]
pub type HostMount = CortexMount;
#[cfg(feature = "fuse-t")]
pub type HostMount = FuseTMount;
