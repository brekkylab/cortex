//! Trees a store describes in-process, and trees the operating system has mounted.
//!
//! * [`FileSystem`]: the one contract a store implements, addressed by path. Stores range from
//!   an in-memory tree to an object store.
//! * [`Posix`]: a [`FileSystem`] in the inode numbers and file handles a kernel speaks in.
//! * [`Directory`]: in-memory files with host directories grafted in; itself a [`FileSystem`].
//! * [`Mount`]: a path on this host where a kernel will answer, implemented by the guard a
//!   binding returns: `FuseMount` (kernel FUSE), `FuseTMount` (FUSE-T's transports),
//!   `DokanMount` (Windows volume via Dokany). There is one binding per filesystem interface,
//!   each only translating its calls onto [`Posix`] (interfaces addressing files by number and
//!   descriptor) or [`FileSystem`] (by path).
//!
//! **A [`FileSystem`] describes a tree; a [`Mount`] is one the operating system has**, and a
//! real mount is the only way out: a guest gets a tree by having it mounted on the host and
//! passed in as a directory, so there is no second, VM-shaped path through [`Posix`].
//!
//! Which one a parameter is typed as is a statement: [`FileSystem`] asks only for a tree the
//! callee addresses by path in-process; [`Mount`] asks for one the caller already mounted,
//! which `std::fs`, a spawned program or a guest can open by name. So
//! [`ConsoleClientBuilder::mount`] takes a [`Mount`]: a session's commands open its context by
//! name.
//!
//! [`ConsoleClientBuilder::mount`]: crate::console::ConsoleClientBuilder::mount

mod directory;
mod filesystem;
mod mount;

pub use directory::*;
pub use filesystem::*;
pub use mount::*;
