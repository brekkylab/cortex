//! Expose any path-addressed store as a real filesystem.
//!
//! A store implements one trait — [`Mountable`] — and a binding puts it in front
//! of a concrete interface. The contract names no interface, so one backend
//! serves a host mount, a guest microVM, or a consumer that addresses files by
//! path.
//!
//! # Using one
//!
//! Pick a backend — [`InMemVolume`], [`PassthroughVolume`], `S3Volume`, or
//! [`Workspace`] to graft several under one tree — and hand it to a binding.
//! `HostMount` is whichever host binding the build enabled; `msb_krun`'s
//! `FsBuilder::custom` wants the same backend wrapped in [`PosixFs`] first. No
//! binding is enabled by default. `examples/mount_host.rs` and
//! `src/bin/apply_krun.rs` walk those two paths end to end.
//!
//! # Implementing one
//!
//! [`Mountable`] is the whole contract: seven operations addressed by path, plus a
//! [`FileHandle`] for offset I/O. [`PosixFs`] is not part of it — that is the
//! translation a *kernel* needs, because kernels address files by number — so an
//! implementor can ignore it.
//!
//! `ARCHITECTURE.md` has the long form.

mod demo;
mod error;
mod executable;
mod lock;
mod mountable;
mod stat;
#[cfg(test)]
mod test_support;
mod wire;
mod workspace;

pub use error::*;
pub use executable::*;
pub use mountable::*;
pub use stat::*;
pub use wire::*;
pub use workspace::*;
