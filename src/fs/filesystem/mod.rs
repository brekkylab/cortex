//! [`FileSystem`] and the stores that implement it, and [`Posix`], the inode layer over it.

mod filesystem;
mod r#impl;
// `fs`-wide because bindings need its attribute layout, errno table and open-flag decoding,
// which are neither a store's nor a consumer's business.
pub(super) mod posix;

pub use filesystem::*;
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
