//! One module per concrete filesystem interface, each binding that interface to the layer
//! below it.
//!
//! A binding only translates: it decodes the interface's arguments, calls one of the
//! shared operations, and encodes the reply. The two things a binding decides for itself
//! are the errno numbering its consumer expects (a guest kernel is always Linux; the
//! host's is the host's) and the concrete attribute type it must fill.
//!
//! **Which layer it binds to is decided by how the interface addresses a file**, and is the
//! one structural difference between the modules here. The FUSE bindings speak inode numbers
//! and file handles, so they go through [`Posix`](crate::fs::Posix), which is where those
//! exist. [`dokan`] does not — the NT I/O manager resolves names itself and every callback
//! carries a whole path — so it reaches [`FileSystem`](crate::fs::FileSystem) directly, which
//! is what `Posix`'s own doc says a path-addressed consumer should do.
//!
//! What a binding exports is its *call surface*, and every one of them has the same shape: a
//! guard whose `try_new` mounts, whose `join` waits for the mount to end, and whose `Drop`
//! takes it down. Nothing else here is public — the vtables, the callbacks and the session
//! handles are each binding's own business.
//!
//! [`Mount`](super::Mount) is that shape as a trait, and sits beside this module rather than
//! in it: what a mount *is* outlives which interface happened to make one, and a consumer that
//! takes a tree names the trait without any binding being compiled in at all.
//!
//! A binding is also partly a trait impl, so declaring the module is what pulls it in.

// One binding per target, all behind `mount`: which interface a host mounts through is
// decided by its OS, so a build never has two to choose between.
#[cfg(all(feature = "mount", windows))]
mod dokan;
#[cfg(all(feature = "mount", unix, not(target_os = "macos")))]
mod fuse;
#[cfg(all(feature = "mount", target_os = "macos"))]
mod fuse_t;

// `self::` because this module and the crate it binds share a name, and a bare `dokan::` in a
// `use` is the *crate*. Both spellings appear below on purpose.
#[cfg(all(feature = "mount", windows))]
pub use self::dokan::DokanMount;
// Re-exported for the same reason `MountOption` is below: a caller names its mount flags
// without taking a direct dependency on `dokan`, which is this binding's implementation detail.
#[cfg(all(feature = "mount", windows))]
pub use ::dokan::MountFlags;
#[cfg(all(feature = "mount", unix, not(target_os = "macos")))]
pub use fuse::FuseMount;
// Re-exported so a caller can name mount options without taking a direct dependency on
// `fuser`, which is this binding's implementation detail.
#[cfg(all(feature = "mount", target_os = "macos"))]
pub use fuse_t::{FuseTBackend, FuseTMount};
#[cfg(all(feature = "mount", unix, not(target_os = "macos")))]
pub use fuser::MountOption;

/// Drive an async [`Posix`](crate::fs::Posix) operation to completion from a binding's
/// *synchronous* callback.
///
/// This is the one place the sync↔async boundary is crossed. A libfuse loop calls the binding
/// on its own thread — never a Tokio worker — while [`Posix`](crate::fs::Posix) and the stores
/// beneath it are async. The bindings block here at their callback boundary rather than
/// embedding a runtime in every leaf store; an async-native frontend (WebDAV/HTTP) drives the
/// same stores with no `block_on` at all.
///
/// One runtime serves every mount this module makes, created on first use and **never
/// dropped** — a `Runtime`'s `Drop` blocks, which would panic on the binding threads that
/// reach this. Created lazily, so a process that mounts nothing pays for nothing.
#[cfg(all(feature = "mount", any(unix, windows)))]
pub(crate) fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::sync::OnceLock;
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build cortex binding runtime")
    })
    .block_on(fut)
}
