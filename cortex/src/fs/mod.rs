//! A path-addressed store ([`FileSystem`]), the POSIX layer that gives it inode identity
//! ([`Posix`]), and the bindings that put one in front of a real filesystem ([`Mount`]).
//!
//! The three form a stack, and the module tree mirrors it:
//!
//! * [`filesystem`] — what a store must provide, and the vocabulary it answers in: a namespace
//!   plane and a data plane, both addressed by path, and nothing that stands for an open.
//!   Descriptor identity belongs to the layer that has descriptors, which is why a store never
//!   sees one.
//! * [`posix`] — [`Posix`], which turns those paths into the inode numbers, file handles
//!   and open decomposition a kernel speaks in, and owns everything the bindings share.
//! * [`mounts`] — the bindings, one per concrete filesystem interface, each doing nothing but
//!   translating that interface's calls onto [`Posix`], plus [`Mount`]: what a live one is.
//! * [`stores`] — the concrete stores, from an in-memory tree to an object store.
//!
//! Beside the stack sits [`workfs`] — [`WorkFs`], which grafts several stores under one tree
//! and is itself a [`FileSystem`], so it is both a consumer of the trait and something the
//! bindings can drive like any single store.
//!
//! **A [`FileSystem`] describes a tree; a [`Mount`] is one the operating system has.** The
//! first is a contract some type implements and nothing outside this process can see; the
//! second is a path where a kernel will answer, made by a binding and taken down when the
//! guard is dropped. A binding exports one such guard: `FuseMount` for the kernel's FUSE,
//! `FuseTMount` for the transports FUSE-T's helper carries. Constructing one mounts, dropping
//! it unmounts, and nothing else about the mount is reachable.
//!
//! **A real mount is the only way out.** A guest that needs one of these trees gets it the way
//! anything else does — mounted on the host, then passed in as a directory — so there is no
//! second, VM-shaped path through [`Posix`] to keep in step with this one.
//!
//! Errors are [`std::io::Error`], classified by kind. There is no error type here to
//! learn: a store's own `std::fs` calls travel up unchanged, and `FileSystem`'s docs say
//! which kind answers what.

mod filesystem;
mod mounts;
mod posix;
mod stores;
mod workfs;

pub use filesystem::*;
// `Mount` is here whatever this build can mount: a consumer takes one to be told where a tree
// is, and which binding made it is the mounting caller's business. The guards themselves are
// each gated on the binding that exports them.
pub use mounts::Mount;
#[cfg(feature = "fuse")]
pub use mounts::{FuseMount, MountOption};
#[cfg(feature = "fuse-t")]
pub use mounts::{FuseTBackend, FuseTMount};
pub use posix::*;
pub use stores::*;
pub use workfs::*;
