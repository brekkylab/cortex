//! Concrete [`FileSystem`](super::FileSystem) stores.
//!
//! The `std`-only stores are always built. Each network store is behind its own feature: `s3`
//! alone adds over a hundred crates, and a consumer serving local files should not compile an
//! HTTP and TLS stack.

#[cfg(feature = "gdrive")]
mod gdrive;
mod inmem;
#[cfg(feature = "notion")]
mod notion;
#[cfg(feature = "onedrive")]
mod onedrive;
mod passthrough;
#[cfg(feature = "s3")]
mod s3;

#[cfg(feature = "gdrive")]
pub use gdrive::*;
pub use inmem::*;
#[cfg(feature = "notion")]
pub use notion::*;
#[cfg(feature = "onedrive")]
pub use onedrive::*;
pub use passthrough::*;
#[cfg(feature = "s3")]
pub use s3::*;
