//! Codec helpers shared by several message types; no protocol rules live here.
//!
//! - `bytes` — a byte payload carried as BSON `Binary` (exec output, read and write data).
//! - `flatten` — writing a value's members into an object someone else opened, so
//!   [`Call`](super::Call) and [`Response`](super::Response) land beside `jsonrpc`.

pub(super) mod bytes;
pub(super) mod flatten;
