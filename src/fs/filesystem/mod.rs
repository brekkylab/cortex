//! [`FileSystem`] (a tree described by path), the [`Posix`] layer that gives it inode
//! identity, and the concrete stores that implement it.
//!
//! [`FileSystem`] is what a store answers, and all an implementor writes: a namespace plane and
//! a data plane, both addressed by path, with nothing that stands for an open. [`Posix`] is what
//! a kernel is told: it sits behind the trait, turns paths into inode numbers, file handles and
//! open decomposition, and owns everything the bindings share. No store names it, so descriptor
//! identity lives in one place.
//!
//! Stores are feature-gated: the network ones are re-exported behind features, so a consumer
//! serving local files compiles none of them.

mod filesystem;
mod r#impl;
// `fs`-wide because bindings need its attribute layout, errno table and open-flag decoding,
// which are neither a store's nor a consumer's business.
pub(super) mod posix;

pub use filesystem::*;
// Network stores are each behind a non-default feature, so a local-files build compiles no
// HTTP and TLS stack.
#[cfg(feature = "gdrive")]
pub use r#impl::{GdriveConfig, GdriveFs, GdriveOrigins};
pub use r#impl::{InMemFs, PassthroughFs};
#[cfg(feature = "notion")]
pub use r#impl::{NotionConfig, NotionFs};
#[cfg(feature = "onedrive")]
pub use r#impl::{OnedriveConfig, OnedriveFs, OnedriveOrigins};
#[cfg(feature = "s3")]
pub use r#impl::{S3Config, S3Fs};
pub use posix::*;
