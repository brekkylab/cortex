//! One module per concrete filesystem interface, each binding
//! [`PosixFs`](super::PosixFs) to that interface.
//!
//! A binding only translates: it decodes the interface's arguments, calls one
//! of the shared operations, and encodes the reply. The two things a binding
//! decides for itself are the errno numbering its consumer expects (a guest
//! kernel is always Linux; the host's is the host's) and the concrete attribute
//! type it must fill.
//!
//! A binding is a trait impl, so declaring the module is what pulls it in. What
//! is re-exported is each binding's *call surface* — the type a program holds to
//! put the filesystem in front of that interface. The krun binding needs none:
//! its call surface belongs to `msb_krun` (`VmBuilder::fs(..).custom(..)`).

#[cfg(feature = "fuse")]
mod fuse;
#[cfg(feature = "fuse-t")]
mod fuse_t;
// Gated like the others even though it exports nothing. A binding is a trait
// impl, so the module declaration is the only thing pulling it in — and an
// ungated one makes its foreign crate mandatory for every consumer, whether or
// not they ever boot a VM.
#[cfg(feature = "krun")]
mod krun;

#[cfg(feature = "fuse")]
pub use fuse::CortexMount;
#[cfg(feature = "fuse-t")]
pub use fuse_t::FuseTMount;
// Re-exported so a caller can name mount options without taking a direct
// dependency on `fuser`, which is our implementation detail.
#[cfg(feature = "fuse")]
pub use fuser::MountOption;

/// The host binding this build has.
///
/// Exported because a consumer cannot express the condition: features are a
/// crate-level concept, so "whichever of the two is enabled" has no spelling
/// outside the crate that defines them.
///
/// FUSE-T wins a tie, needing no kernel extension.
#[cfg(all(feature = "fuse", not(feature = "fuse-t")))]
pub type HostMount = CortexMount;
#[cfg(feature = "fuse-t")]
pub type HostMount = FuseTMount;

/// Drive an async [`PosixFs`](super::PosixFs) operation to completion from a
/// binding's *synchronous* callback.
///
/// This is the one place the sync↔async boundary is crossed. A guest kernel
/// (krun) or a libfuse loop (fuse/fuse-t) calls the binding on its own thread —
/// never a Tokio worker — while `PosixFs` and the backends beneath it are async.
/// The bindings block here at their callback boundary rather than embedding a
/// runtime in every leaf backend; an async-native frontend (WebDAV/HTTP) drives
/// the same backends with no `block_on` at all.
///
/// One process-wide runtime serves every mount, created on first use and **never
/// dropped** — a `Runtime`'s `Drop` blocks, which would panic on the binding
/// threads that reach this.
#[cfg(any(feature = "krun", feature = "fuse", feature = "fuse-t"))]
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
