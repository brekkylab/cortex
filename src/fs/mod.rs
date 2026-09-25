//! Two traits and one type. [`FileSystem`] is what a store must provide, [`Mount`] is what the
//! host has once one of those trees is attached to it, and [`Directory`] is the public API — the
//! tree a caller actually assembles and hands to a binding.
//!
//! * [`FileSystem`] — the one contract a store implements, and the vocabulary it answers in: a
//!   namespace plane and a data plane, both addressed by path, and nothing that stands for an
//!   open. Descriptor identity belongs to the layer that has descriptors, which is [`Posix`]:
//!   it turns those paths into the inode numbers, file handles and open decomposition a kernel
//!   speaks in, and owns everything the bindings share, so a store never sees one. The concrete
//!   stores sit under the trait, from an in-memory tree to an object store.
//! * [`Mount`] — the state of having been mounted, which is a path on this host where a kernel
//!   will answer. What implements it is the guard a binding hands back, and the bindings are
//!   one per concrete filesystem interface, each doing nothing but translating that interface's
//!   calls onto the layer below it — [`Posix`] for an interface that addresses files by number
//!   and descriptor, [`FileSystem`] itself for one that addresses them by path.
//! * [`Directory`] — files held in memory with host directories grafted in beside them, and
//!   itself a [`FileSystem`], so it is both a consumer of the trait and something the bindings
//!   can drive like any single store.
//!
//! **A [`FileSystem`] describes a tree; a [`Mount`] is one the operating system has.** The
//! first is a contract some type implements and nothing outside this process can see; the
//! second is a path where a kernel will answer, made by a binding and taken down when the
//! guard is dropped. A binding exports one such guard: `FuseMount` for the kernel's FUSE,
//! `FuseTMount` for the transports FUSE-T's helper carries, `DokanMount` for a Windows volume
//! through Dokany. Constructing one mounts, dropping it unmounts, and nothing else about the
//! mount is reachable.
//!
//! **A real mount is the only way out.** A guest that needs one of these trees gets it the way
//! anything else does — mounted on the host, then passed in as a directory — so there is no
//! second, VM-shaped path through [`Posix`] to keep in step with this one.
//!
//! Which of the two a consumer names is therefore a statement, and worth reading as one. A
//! parameter typed [`FileSystem`] asks only to be *given a tree*: the callee will address it by
//! path, inside this process, and does not care whether anything outside can see it. A
//! parameter typed [`Mount`] asks for a tree the host already has — the caller must have
//! mounted it, and what arrives is a directory that `std::fs`, a spawned program or a guest can
//! open by name. [`Console`] takes the second: what a session names as its context is what a
//! command it runs will open by name, so its signature says "a tree plugged into this host"
//! rather than "a tree".
//!
//! Errors are [`std::io::Error`], classified by kind. There is no error type here to
//! learn: a store's own `std::fs` calls travel up unchanged, and `FileSystem`'s docs say
//! which kind answers what.
//!
//! [`Console`]: crate::console::Console

mod directory;
mod filesystem;
mod mount;

pub use directory::*;
pub use filesystem::*;
pub use mount::*;
