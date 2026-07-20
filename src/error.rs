//! Error type and result alias shared by all VFS backends.

/// Errors that any VFS backend can produce.
#[derive(Debug)]
pub enum VfsError {
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

impl std::fmt::Display for VfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VfsError::NotFound => write!(f, "no such entry"),
            VfsError::NotADirectory => write!(f, "not a directory"),
            VfsError::IsADirectory => write!(f, "is a directory"),
            VfsError::AlreadyExists => write!(f, "entry already exists"),
            VfsError::InvalidName => write!(f, "invalid name"),
            VfsError::Unsupported => write!(f, "operation not supported"),
            VfsError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for VfsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VfsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for VfsError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::NotFound => VfsError::NotFound,
            ErrorKind::AlreadyExists => VfsError::AlreadyExists,
            _ => VfsError::Io(e),
        }
    }
}

pub type Result<T> = std::result::Result<T, VfsError>;
