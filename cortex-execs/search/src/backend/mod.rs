//! One module per lane that has an index.
//!
//! A backend knows one service's index and one tree's layout, and nothing about the command in
//! front of it. Each is behind a feature, for the reason `cortex` puts its stores behind
//! features: a build that mounts chat has no reason to compile a mail client.

#[cfg(feature = "messenger")]
pub mod messenger;
