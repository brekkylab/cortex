//! [`FileSystem`] — a tree described by path — the POSIX layer that gives one inode identity,
//! and the concrete stores that implement it.
//!
//! The split is between what a store must answer and what a kernel must be told. [`FileSystem`]
//! is the first, and is the whole of what an implementor writes: a namespace plane and a data
//! plane, both addressed by path, with nothing that stands for an open. [`Posix`] is the
//! second, and sits behind the trait rather than beside it — it turns those paths into the
//! inode numbers, file handles and open decomposition a kernel speaks in, and owns everything
//! the bindings share. A store never names it, which is why descriptor identity can live in one
//! place instead of in every store.
//!
//! The stores themselves are implementations of the trait and nothing more, so which of them a
//! build has is a feature: the ones that reach the network are re-exported gated, and a
//! consumer serving local files compiles neither.

mod filesystem;
mod r#impl;
// Visible to the whole `fs` subtree, because the bindings marshal: they need the attribute
// layout, the errno table and the open-flag decoding that `Posix` answers a kernel with, and
// none of that is a store's business or a consumer's.
pub(super) mod posix;

pub use filesystem::*;
pub use posix::*;
// A store that reaches the network is behind its own feature, for the reason the crate has no
// default features at all: `s3` costs 131 crates, and a build that serves local files should
// not compile an HTTP and TLS stack to do it.
pub use r#impl::{InMemFs, PassthroughFs};
#[cfg(feature = "notion")]
pub use r#impl::{NotionConfig, NotionFs};
#[cfg(feature = "s3")]
pub use r#impl::{S3Config, S3Fs};
