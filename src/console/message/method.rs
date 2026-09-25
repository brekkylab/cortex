//! The name a method goes by on the wire.
//!
//! Its own file because all three of [`Call`](super::Call),
//! [`Notification`](super::Notification) and [`Response`](super::Response) name it, and
//! none of them owns it: which methods exist is the protocol's, and which side a given
//! one is on is [`is_notification`](Method::is_notification).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Which method a request called, and its response answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Init,
    Exec,
    Read,
    Write,
    Snapshot,
    Start,
    Stop,
    Quit,
}

impl Method {
    /// The name as it appears in a `method` member.
    pub fn as_str(&self) -> &'static str {
        match self {
            Method::Snapshot => "snapshot",
            Method::Init => "init",
            Method::Exec => "exec",
            Method::Read => "read",
            Method::Write => "write",
            Method::Start => "start",
            Method::Stop => "stop",
            Method::Quit => "quit",
        }
    }

    /// Whether this method is one nothing answers.
    ///
    /// Which side a method is on is what JSON-RPC's `id` decides, and it is a property
    /// of the method rather than of the message carrying it — so both halves ask here
    /// rather than each keeping its own list.
    pub fn is_notification(&self) -> bool {
        matches!(self, Method::Start | Method::Stop | Method::Quit)
    }

    pub fn parse(name: &str) -> Option<Method> {
        Some(match name {
            "snapshot" => Method::Snapshot,
            "init" => Method::Init,
            "exec" => Method::Exec,
            "read" => Method::Read,
            "write" => Method::Write,
            "start" => Method::Start,
            "stop" => Method::Stop,
            "quit" => Method::Quit,
            _ => return None,
        })
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
