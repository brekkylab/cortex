//! Two traits and one type: [`FileSystem`] is what a store provides, [`Mount`] is a tree the
//! host has attached, and [`Directory`] is the tree a caller assembles and hands to a binding.
//!
//! * [`FileSystem`] — the one contract a store implements: a namespace plane and a data plane,
//!   both addressed by path, with nothing that stands for an open. [`Posix`] turns those paths
//!   into the inode numbers, file handles and open decomposition a kernel speaks in, and owns
//!   everything the bindings share, so a store never sees a descriptor. Concrete stores range
//!   from an in-memory tree to an object store.
//! * [`Mount`] — a path on this host where a kernel will answer. It is implemented by the guard
//!   a binding returns. There is one binding per filesystem interface, each only translating
//!   that interface's calls onto [`Posix`] (interfaces addressing files by number and
//!   descriptor) or [`FileSystem`] (interfaces addressing them by path).
//! * [`Directory`] — in-memory files with host directories grafted in beside them; itself a
//!   [`FileSystem`], so bindings drive it like any single store.
//!
//! **A [`FileSystem`] describes a tree; a [`Mount`] is one the operating system has.** The
//! first is invisible outside this process; the second is made by a binding and taken down
//! when its guard drops. Guards: `FuseMount` (kernel FUSE), `FuseTMount` (FUSE-T's transports),
//! `DokanMount` (Windows volume via Dokany). Constructing one mounts, dropping it unmounts, and
//! nothing else about the mount is reachable.
//!
//! **A real mount is the only way out.** A guest gets a tree by having it mounted on the host
//! and passed in as a directory, so there is no second, VM-shaped path through [`Posix`].
//!
//! Which one a parameter is typed as is a statement: [`FileSystem`] asks only for a tree the
//! callee addresses by path in-process; [`Mount`] asks for a tree the caller already mounted,
//! which `std::fs`, a spawned program or a guest can open by name. [`ConsoleClient`] takes a
//! [`Mount`] because a session's context is what its commands open by name.
//!
//! Errors are [`std::io::Error`], classified by kind: a store's own `std::fs` errors travel up
//! unchanged, and `FileSystem`'s docs say which kind answers what.
//!
//! [`ConsoleClient`]: crate::console::ConsoleClient

mod directory;
mod filesystem;
mod mount;

pub use directory::*;
pub use filesystem::*;
pub use mount::*;
