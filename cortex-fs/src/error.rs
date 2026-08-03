//! Error type and result alias shared by all Cortex backends.

/// Errors that any Cortex backend can produce.
#[derive(Debug)]
pub enum CortexError {
    /// No entry exists at the requested name.
    NotFound,
    /// Expected a directory but found a file.
    NotADirectory,
    /// Expected a file but found a directory.
    IsADirectory,
    /// An entry already exists where a new one was requested.
    AlreadyExists,
    /// The name is not a single, valid path component.
    InvalidName,
    /// The backend does not support this operation.
    Unsupported,
    /// An underlying I/O error.
    Io(std::io::Error),
}

impl std::fmt::Display for CortexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CortexError::NotFound => write!(f, "no such entry"),
            CortexError::NotADirectory => write!(f, "not a directory"),
            CortexError::IsADirectory => write!(f, "is a directory"),
            CortexError::AlreadyExists => write!(f, "entry already exists"),
            CortexError::InvalidName => write!(f, "invalid name"),
            CortexError::Unsupported => write!(f, "operation not supported"),
            CortexError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for CortexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CortexError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CortexError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::NotFound => CortexError::NotFound,
            ErrorKind::AlreadyExists => CortexError::AlreadyExists,
            _ => CortexError::Io(e),
        }
    }
}

pub type Result<T> = std::result::Result<T, CortexError>;
