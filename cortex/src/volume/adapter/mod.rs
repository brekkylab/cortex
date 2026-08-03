//! Adapters that expose a [`Mountable`](super::Mountable) backend through a
//! concrete OS-facing interface.

#[cfg(feature = "krun")]
mod krun;
mod posix;

pub use posix::*;
