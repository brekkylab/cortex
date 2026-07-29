//! Concrete [`Mountable`](super::Mountable) backends.

mod inmem;
mod passthrough;
#[cfg(feature = "s3")]
mod s3;

pub use inmem::*;
pub use passthrough::*;
#[cfg(feature = "s3")]
pub use s3::*;
