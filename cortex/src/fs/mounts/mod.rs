//! One module per concrete filesystem interface, each binding [`Posix`](super::Posix) to
//! that interface.
//!
//! A binding only translates: it decodes the interface's arguments, calls one of the
//! shared operations, and encodes the reply. The two things a binding decides for itself
//! are the errno numbering its consumer expects (a guest kernel is always Linux; the
//! host's is the host's) and the concrete attribute type it must fill.
//!
//! What a binding exports is its *call surface*, and every one of them has the same shape: a
//! guard whose `try_new` mounts, whose `join` waits for the mount to end, and whose `Drop`
//! takes it down. Nothing else here is public — the vtables, the callbacks and the session
//! handles are each binding's own business.
//!
//! A binding is also partly a trait impl, so declaring the module is what pulls it in.

#[cfg(feature = "fuse")]
mod fuse;
#[cfg(feature = "fuse-t")]
mod fuse_t;

#[cfg(feature = "fuse")]
pub use fuse::FuseMount;
// Re-exported so a caller can name mount options without taking a direct dependency on
// `fuser`, which is this binding's implementation detail.
#[cfg(feature = "fuse-t")]
pub use fuse_t::{FuseTBackend, FuseTMount};
#[cfg(feature = "fuse")]
pub use fuser::MountOption;

/// Drive an async [`Posix`](super::Posix) operation to completion from a binding's
/// *synchronous* callback.
///
/// This is the one place the sync↔async boundary is crossed. A libfuse loop calls the binding
/// on its own thread — never a Tokio worker — while [`Posix`] and the stores beneath it are
/// async. The bindings block here at their callback boundary rather than embedding a runtime in
/// every leaf store; an async-native frontend (WebDAV/HTTP) drives the same stores with no
/// `block_on` at all.
///
/// One runtime serves every mount this module makes, created on first use and **never
/// dropped** — a `Runtime`'s `Drop` blocks, which would panic on the binding threads that
/// reach this. Created lazily, so a process that mounts nothing pays for nothing.
///
/// [`Posix`]: super::Posix
#[cfg(any(feature = "fuse", feature = "fuse-t"))]
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
