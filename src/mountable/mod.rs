//! A path-addressed backend ([`MountableV2`]) and the adapters that expose it
//! to the outside world.

mod adapters;
mod mountable;

pub use adapters::*;
pub use mountable::*;
