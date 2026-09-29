//! One module per concrete filesystem interface, each binding it to the layer below.
//!
//! A binding only translates: decode the interface's arguments, call a shared operation,
//! encode the reply. It decides only the errno numbering its consumer expects (a guest kernel
//! is always Linux; the host's is the host's) and the attribute type it fills.
//!
//! **How the interface addresses a file decides the layer.** FUSE speaks inode numbers and
//! file handles, so its bindings go through [`Posix`](crate::fs::Posix). [`dokan`] gets a whole
//! path per callback (the NT I/O manager resolves names), so it uses
//! [`FileSystem`](crate::fs::FileSystem) directly.
//!
//! Each binding exports only a guard whose `try_new` mounts, `join` waits for the mount to
//! end, and `Drop` takes it down; vtables, callbacks and session handles stay private.
//! [`Mount`](super::Mount) lives outside this module so consumers can name it with no binding
//! compiled in. A binding is partly a trait impl, so declaring its module pulls it in.

// One binding per target, all behind `mount`: which interface a host mounts through is
// decided by its OS, so a build never has two to choose between.
#[cfg(all(feature = "mount", windows))]
mod dokan;
#[cfg(all(feature = "mount", unix, not(target_os = "macos")))]
mod fuse;
#[cfg(all(feature = "mount", target_os = "macos"))]
mod fuse_t;

// `self::` because a bare `dokan::` in a `use` names the crate, not this module.
#[cfg(all(feature = "mount", windows))]
pub use self::dokan::DokanMount;
// Re-exported so callers need no direct dependency on `dokan`.
#[cfg(all(feature = "mount", windows))]
pub use ::dokan::MountFlags;
#[cfg(all(feature = "mount", unix, not(target_os = "macos")))]
pub use fuse::FuseMount;
#[cfg(all(feature = "mount", target_os = "macos"))]
pub use fuse_t::{FuseTBackend, FuseTMount};
// Re-exported so callers need no direct dependency on `fuser`.
#[cfg(all(feature = "mount", unix, not(target_os = "macos")))]
pub use fuser::MountOption;

/// Drive an async [`Posix`](crate::fs::Posix) operation to completion from a binding's
/// *synchronous* callback.
///
/// Bindings are called on their own threads (never a Tokio worker) while the stores are
/// async, so they block here rather than each store embedding a runtime.
///
/// One lazily created runtime serves every mount and is **never dropped**: a `Runtime`'s
/// `Drop` blocks, which would panic on the binding threads that reach this.
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
