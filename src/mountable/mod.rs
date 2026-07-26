//! A path-addressed backend ([`Mountable`]) and the adapters that expose it
//! to the outside world.

mod adapters;
mod impls;
mod r#trait;

pub use adapters::*;
pub use impls::*;
pub use r#trait::*;
