//! HyperCLOVA X working over a Cortex tree, inside one actor's permissions.
//!
//! [`session::run`] is the whole of it: build the tree for an actor, hand HyperCLOVA X tools
//! over it, and report what happens as [`session::Event`]s. `src/main.rs` prints those events
//! to a terminal; a window can render the same stream.

pub mod acl;
pub mod audit;
pub mod session;
pub mod tools;
pub mod tree;
