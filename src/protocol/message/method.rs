//! The name a method goes by on the wire, shared by [`Call`](super::Call),
//! [`Notification`](super::Notification) and [`Response`](super::Response).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Which method a request called, and its response answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Version,
    BuildImage,
    RemoveImage,
    ListImages,
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
            Method::Version => "version",
            Method::BuildImage => "build_image",
            Method::RemoveImage => "remove_image",
            Method::ListImages => "list_images",
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

    /// Whether nothing answers this method.
    ///
    /// A property of the method, not the message.
    pub fn is_notification(&self) -> bool {
        matches!(self, Method::Start | Method::Stop | Method::Quit)
    }

    pub fn parse(name: &str) -> Option<Method> {
        Some(match name {
            "version" => Method::Version,
            "build_image" => Method::BuildImage,
            "remove_image" => Method::RemoveImage,
            "list_images" => Method::ListImages,
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
