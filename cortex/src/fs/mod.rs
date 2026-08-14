//! A path-addressed store ([`Mountable`]), the POSIX layer that gives it inode identity
//! ([`Posix`]), and the bindings that put one in front of a real filesystem.
//!
//! The three form a stack, and the module tree mirrors it:
//!
//! * [`mountable`] — what a store must provide, and the vocabulary it answers in: a namespace
//!   plane and a data plane, both addressed by path, and nothing that stands for an open.
//!   Descriptor identity belongs to the layer that has descriptors, which is why a store never
//!   sees one.
//! * [`posix`] — [`Posix`], which turns those paths into the inode numbers, file handles
//!   and open decomposition a kernel speaks in, and owns everything the bindings share.
//! * [`mounts`] — the bindings, one per concrete filesystem interface, each doing nothing but
//!   translating that interface's calls onto [`Posix`].
//! * [`stores`] — the concrete stores, from an in-memory tree to an object store.
//!
//! Beside the stack sits [`workfs`] — [`WorkFs`], which grafts several stores under one tree
//! and is itself a [`Mountable`], so it is both a consumer of the trait and something the
//! bindings can drive like any single store.
//!
//! A binding exports one guard type: `FuseMount` for the kernel's FUSE, `FuseTMount` for the
//! transports FUSE-T's helper carries. Constructing one mounts, dropping it unmounts, and
//! nothing else about the mount is reachable.
//!
//! **A real mount is the only way out.** A guest that needs one of these trees gets it the way
//! anything else does — mounted on the host, then passed in as a directory — so there is no
//! second, VM-shaped path through [`Posix`] to keep in step with this one.
//!
//! Errors are [`std::io::Error`], classified by kind. There is no error type here to
//! learn: a store's own `std::fs` calls travel up unchanged, and `Mountable`'s docs say
//! which kind answers what.

mod mountable;
mod mounts;
mod posix;
mod stores;
mod workfs;

// Gated on the bindings that have a call surface to export, because a glob over a module
// that exports nothing is an unused import rather than an empty one.
pub use mountable::*;
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
pub use mounts::*;
pub use posix::*;
pub use stores::*;
pub use workfs::*;
