//! Slack, as a [`MessengerSource`](super::MessengerSource).
//!
//! Two files, because the halves fail differently and are tested differently:
//!
//! * [`accessor`] — the Web API client. One gate on every call, because Slack reports
//!   application errors *inside* a 200; cursor pagination; the one host check an attachment
//!   download is allowed to send the token to.
//! * `source.rs` — which of those calls answers which of the lane's questions, and how a
//!   Slack message becomes a [`Message`](super::Message). Arriving next.
//!
//! # Where the credentials come from
//!
//! [`SlackConfig`](crate::fs::SlackConfig) carries tokens that were already obtained.
//! Getting them — the `oauth.v2.access` code exchange — needs a client secret and belongs to
//! whatever ran the consent flow, which is a server with an HTTP handler and not a
//! filesystem: a [`VolumeSpec`](crate::fs::VolumeSpec) is a description handed across a
//! process boundary, so by the time one names a Slack workspace the consent is over.
//! `S3Config` and `NotionConfig` are the same shape for the same reason.

mod accessor;
mod source;

pub use accessor::*;
pub use source::*;
