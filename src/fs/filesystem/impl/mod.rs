//! Concrete [`FileSystem`](super::FileSystem) stores.
//!
//! The two that need nothing outside `std` are always here. A store that reaches the network is
//! behind its own feature, for the reason the crate has no default features at all: `s3` costs
//! 131 crates, and a consumer serving local files should not compile an HTTP and TLS stack to
//! do it.

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
