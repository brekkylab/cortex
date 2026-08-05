//! Concrete [`Mountable`](super::Mountable) backends.
//!
//! The two that need nothing outside `std` are always here. A backend that reaches
//! the network is behind its own feature, for the reason the crate has no default
//! features at all: `s3` costs 131 crates, and a consumer serving local files should
//! not compile an HTTP and TLS stack to do it.

mod inmem;
mod passthrough;
#[cfg(feature = "s3")]
mod s3;

pub use inmem::*;
pub use passthrough::*;
#[cfg(feature = "s3")]
pub use s3::*;
