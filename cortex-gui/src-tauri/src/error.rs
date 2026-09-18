//! The one error a command answers in.
//!
//! Everything below this crate already speaks [`std::io::Error`] — `cortex` says so on
//! purpose, so its two halves share a vocabulary — and a Tauri command has to hand the
//! webview something `serde` can write. So this is that conversion and nothing else: a
//! string carrying the kind alongside the message, because a webview showing only
//! "entity not found" leaves the reader guessing which of the three paths in the form
//! was the missing one, while the kind at least says what sort of refusal it was.

use std::{fmt, io};

/// A failure on its way to the window.
#[derive(Debug)]
pub struct Error(String);

impl Error {
    /// A refusal this crate decided on itself, with no `io::Error` behind it.
    pub fn msg(message: impl Into<String>) -> Self {
        Error(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error(format!("{} ({:?})", err, err.kind()))
    }
}

impl serde::Serialize for Error {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// What every `#[tauri::command]` in this crate returns.
pub type Result<T> = std::result::Result<T, Error>;
