//! The wire: JSON-RPC 2.0's object model, encoded as BSON. The protocol itself (methods,
//! errors, design) is described on [`protocol`](super).
//!
//! - `message` — the envelope: [`Message`] and the [`RequestId`] that pairs a response
//!   with its request.
//! - `call` — [`Call`] and [`Response`], with one file per method holding its params and
//!   result types ([`InitCall`]/[`InitResp`], [`ExecCall`]/[`ExecResp`], ...).
//! - `notification` — [`Notification`], the unanswered methods.
//! - `method` — [`Method`], a method's wire name.
//! - `error` — [`Error`] and its codes.
//! - `utils` — codec helpers (`bytes`, `flatten`).

mod call;
mod error;
mod message;
mod method;
mod notification;
mod utils;

pub use call::*;
pub use error::*;
pub use message::*;
pub use method::*;
pub use notification::*;

/// The version of this protocol, which `version` answers with.
pub const PROTOCOL_VERSION: &str = "1";
