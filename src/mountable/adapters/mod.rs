//! Adapters that expose a [`MountableV2`](super::MountableV2) backend through a
//! concrete OS-facing interface.

mod krun;
mod posix;

pub use posix::PosixAdapter;
