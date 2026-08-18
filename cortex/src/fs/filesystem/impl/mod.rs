//! Concrete [`FileSystem`](super::FileSystem) stores.
//!
//! The two that need nothing outside `std` are always here. A store that reaches the network is
//! behind its own feature, for the reason the crate has no default features at all: `s3` costs
//! 131 crates, and a consumer serving local files should not compile an HTTP and TLS stack to
//! do it.

mod inmem;
// A directory, and one *family* of stores rather than one: a messenger has no hierarchy to
// mirror, so its tree is synthesized — once, shared by every platform in the lane.
#[cfg(feature = "slack")]
mod messenger;
#[cfg(feature = "notion")]
mod notion;
mod passthrough;
#[cfg(feature = "s3")]
mod s3;

pub use inmem::*;
#[cfg(feature = "slack")]
pub use messenger::*;
#[cfg(feature = "notion")]
pub use notion::*;
pub use passthrough::*;
#[cfg(feature = "s3")]
pub use s3::*;
