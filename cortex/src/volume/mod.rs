//! A path-addressed backend ([`Mountable`]) and the adapters that expose it
//! to the outside world.

mod adapter;
mod r#impl;
mod stat;
mod r#trait;
mod volume;

pub use adapter::*;
pub use r#impl::*;
pub use stat::*;
pub use r#trait::*;
