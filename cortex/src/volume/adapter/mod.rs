//! Adapters that expose a [`Mountable`](super::Mountable) backend through a
//! concrete OS-facing interface.

#[cfg(feature = "uvm")]
mod krun;
mod posix;

pub use posix::*;
