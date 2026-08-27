//! The messenger lane: work chat as a tree an agent can `grep`.
//!
//! One [`FileSystem`](crate::fs::FileSystem) implementation ([`MessengerFs`]) over any number
//! of [`MessengerSource`]s. That split is the whole design, and it is the answer to a specific
//! failure: a messenger has no hierarchy to mirror — S3 has keys, Notion a page tree, Drive
//! folders — so its tree has to be *synthesized* along the time axis, and synthesizing it
//! once per platform is how four adapters end up with four layouts that only a document
//! claims are the same.
//!
//! So the platform-specific half is deliberately small:
//!
//! | | where | what varies |
//! | --- | --- | --- |
//! | HTTP client | `<platform>/accessor.rs` | everything — auth, error convention, paging |
//! | source | `<platform>/source.rs` | which call answers which question |
//! | **retry arithmetic** | [`retry`] | **nothing** |
//! | **line format** | [`render_line`] | **nothing** |
//! | **tree layout** | `messenger.rs` | **nothing** |
//!
//! The three "nothing" rows are what make the lane a standard rather than a convention. A
//! layout two implementations agree to by hand can drift by hand; a single renderer and a
//! single tree cannot.
//!
//! # Scope
//!
//! Read-only, and only conversations that are *streams* — Slack channels and DMs, Discord
//! text channels, Teams 1:1/group chats, Google Chat spaces. A forum (Teams channel posts,
//! Discord forum channels, GitHub issues) is addressed by item id rather than by time, keeps
//! a bounded index instead of a date walk, and shares no line format with any of the above —
//! so it is a different shape with a different home, not a fifth source here.

mod error;
mod limits;
mod messenger;
mod paths;
mod retry;
mod search;
mod source;

#[cfg(feature = "slack")]
mod slack;

pub use error::*;
pub use limits::*;
pub use messenger::*;
pub use paths::*;
pub use source::*;

#[cfg(feature = "slack")]
pub use slack::*;
