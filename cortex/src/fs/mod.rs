//! Two traits and one type. [`FileSystem`] is what a store must provide, [`Mount`] is what the
//! host has once one of those trees is attached to it, and [`WorkFs`] is the public API — the
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
//!   calls onto [`Posix`].
//! * [`WorkFs`] — several stores grafted under one tree, and itself a [`FileSystem`], so it is
//!   both a consumer of the trait and something the bindings can drive like any single store.
//!   A caller that has one store still has a tree; a caller that has five has this.
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
//! Which of the two a consumer names is therefore a statement, and worth reading as one. A
//! parameter typed [`FileSystem`] asks only to be *given a tree*: the callee will address it by
//! path, inside this process, and does not care whether anything outside can see it. A
//! parameter typed [`Mount`] asks for a tree the host already has — the caller must have
//! mounted it, and what arrives is a directory that `std::fs`, a spawned program or a guest can
//! open by name. [`Executable`] takes the second: it is handed a path its caller used and has to
//! open the file its caller meant, so its signature says "a tree plugged into this host" rather
//! than "a tree".
//!
//! Errors are [`std::io::Error`], classified by kind. There is no error type here to
//! learn: a store's own `std::fs` calls travel up unchanged, and `FileSystem`'s docs say
//! which kind answers what.
//!
//! [`Executable`]: crate::exec::Executable

mod filesystem;
mod mount;
mod workfs;

pub use filesystem::*;
pub use mount::*;
pub use workfs::*;
