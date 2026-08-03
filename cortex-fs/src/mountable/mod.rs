//! A path-addressed backend ([`Mountable`]) and the adapters that expose it
//! to the outside world.

mod adapter;
mod r#impl;
mod r#trait;

pub use adapter::*;
pub use r#impl::*;
pub use r#trait::*;
