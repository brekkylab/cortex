//! How cortex failures reach JavaScript, from consoles and image clients alike: an `Error`
//! whose `code` names the kind, as Node's own errors carry `ENOENT`.
//!
//! - A server refusal's code is cortex's name for its number (`TIMED_OUT`, `NOT_FOUND`), so
//!   callers compare readable strings; a number with no name is `CONSOLE_REFUSED`, with the
//!   number in the message.
//! - A broken channel is `CONSOLE_BROKEN`; nothing more will be heard on it.
//! - Anything else (building a console, say) is `CORTEX_ERROR`.
//! - Filesystem errors are [`std::io::Error`], coded by their [`ErrorKind`](std::io::ErrorKind)'s
//!   name.

use cortex::protocol::{Error, Failure};

pub type Result<T> = napi::Result<T, String>;

pub fn failure(failure: Failure) -> napi::Error<String> {
    match failure {
        Failure::Refused(error) => match name(error.code) {
            Some(name) => napi::Error::new(name.to_string(), error.message),
            None => napi::Error::new(
                "CONSOLE_REFUSED".to_string(),
                format!("{} ({})", error.message, error.code),
            ),
        },
        Failure::Broken(error) => {
            napi::Error::new("CONSOLE_BROKEN".to_string(), format!("{error:#}"))
        }
    }
}

/// Building a console may fail with a [`Failure`] underneath (the server refusing `init`),
/// which keeps its code as any refusal does.
pub fn anyhow(error: anyhow::Error) -> napi::Error<String> {
    match error.downcast::<Failure>() {
        Ok(f) => failure(f),
        Err(error) => napi::Error::new("CORTEX_ERROR".to_string(), format!("{error:#}")),
    }
}

pub fn io(error: std::io::Error) -> napi::Error<String> {
    napi::Error::new(format!("{:?}", error.kind()), error.to_string())
}

pub fn invalid(reason: impl ToString) -> napi::Error<String> {
    napi::Error::new("INVALID_ARG".to_string(), reason)
}

fn name(code: i64) -> Option<&'static str> {
    Some(match code {
        Error::TIMED_OUT => "TIMED_OUT",
        Error::NOT_EXECUTABLE => "NOT_EXECUTABLE",
        Error::BOOT_FAILED => "BOOT_FAILED",
        Error::NOT_FOUND => "NOT_FOUND",
        Error::IS_A_DIRECTORY => "IS_A_DIRECTORY",
        Error::IO_FAILED => "IO_FAILED",
        Error::UNSUPPORTED_MOUNT => "UNSUPPORTED_MOUNT",
        Error::MOUNT_FAILED => "MOUNT_FAILED",
        Error::UNSUPPORTED_NETWORK => "UNSUPPORTED_NETWORK",
        Error::UNSUPPORTED_IMAGE => "UNSUPPORTED_IMAGE",
        Error::UNKNOWN_IMAGE => "UNKNOWN_IMAGE",
        Error::UNSUPPORTED_MACHINE => "UNSUPPORTED_MACHINE",
        Error::INVALID_REQUEST => "INVALID_REQUEST",
        Error::METHOD_NOT_FOUND => "METHOD_NOT_FOUND",
        Error::INVALID_PARAMS => "INVALID_PARAMS",
        Error::INTERNAL_ERROR => "INTERNAL_ERROR",
        _ => return None,
    })
}

/// A JavaScript number where cortex takes a `u64` (offset, length, timeout).
///
/// napi converts numbers to `i64`; a negative one is rejected here rather than wrapped into a
/// huge length.
pub fn unsigned(value: Option<i64>, what: &str) -> Result<Option<u64>> {
    value
        .map(|v| u64::try_from(v).map_err(|_| invalid(format!("{what} must not be negative"))))
        .transpose()
}
