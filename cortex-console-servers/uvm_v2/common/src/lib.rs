//! The names the three halves of the micro-VM console server share.
//!
//! Every item is part of some cross-process contract — see [`contract`]. Nothing here runs
//! anything: the host, the boot and the guest each hold their own half of the work, and this
//! is only what they have to agree on to hand it over.

pub mod contract;
