//! A path-addressed backend ([`Mountable`]), the POSIX layer that gives it
//! inode identity, and the adapters that expose that layer to the outside
//! world.
//!
//! The three form a stack, and the module tree mirrors it:
//!
//! * [`r#trait`] — what a backend must provide (paths in, bytes out).
//! * [`posix`] — [`PosixFs`], which translates those paths into the inodes and
//!   open handles a kernel speaks in, and owns the operations both bindings
//!   share.
//! * [`adapter`] — one module per concrete filesystem interface, each doing
//!   nothing but translating that interface's calls onto [`PosixFs`].
//!
//! Beside the stack sit the two things every layer of it speaks in: [`stat`], the
//! metadata a backend answers with, and [`workspace`], which grafts several
//! backends under one tree and is itself a [`Mountable`].
//!
//! [`spec`] is the same tree written down: a [`WorkspaceSpec`] is plain data, so a
//! caller that builds its workspace in one process and needs it in another sends
//! the description rather than the thing.
//!
//! `ARCHITECTURE.md` in this directory has the long form.

// The bindings themselves are trait impls, which apply wherever `PosixFs` and
// the foreign trait are both in scope — declaring the module is what pulls them
// into the crate. What `adapter` does export is each binding's call surface.
mod adapter;
mod r#impl;
mod posix;
mod spec;
mod stat;
mod r#trait;
mod workspace;

// Feature-gated because only the host-side bindings have a call surface of their
// own to export — the krun binding's belongs to `msb_krun`.
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
pub use adapter::*;
pub use r#impl::*;
pub use posix::*;
pub use spec::*;
pub use stat::*;
pub use r#trait::*;
pub use workspace::*;
