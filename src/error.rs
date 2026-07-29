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
    /// A directory still has children. Emptying it is the caller's job: a kernel
    /// decomposes `rm -rf` into `unlink`s and a final `rmdir`, and never asks a
    /// filesystem to delete recursively.
    NotEmpty,
    /// The name is not a single, valid path component.
    InvalidName,
    /// The arguments are self-contradictory (e.g. an open that asks for neither
    /// read nor write, or a flags word with an impossible access mode).
    InvalidArgument,
    /// The requested size or offset exceeds what the backend will represent.
    ///
    /// A guest chooses the offset of every write and virtio-fs caps the byte count
    /// but not the offset, so a backend that sizes an allocation from an offset
    /// needs a ceiling — otherwise the guest picks how much host memory to ask
    /// for.
    FileTooLarge,
    /// The file handle is not one this filesystem issued, or has been released.
    ///
    /// Distinct from [`NotFound`](Self::NotFound), which is about a *name*: a
    /// caller told "no such file" for a closed descriptor retries the open
    /// forever, where "bad descriptor" makes it fix its own bookkeeping.
    BadHandle,
    /// The caller is not permitted to do this.
    PermissionDenied,
    /// The backing store is out of space.
    NoSpace,
    /// The backend is mounted or configured read-only.
    ///
    /// Not [`Unsupported`](Self::Unsupported): a FUSE kernel that gets `ENOSYS`
    /// disables that operation for the *whole mount*, so a read-only backend
    /// answering `Unsupported` to a write takes every other backend's writes with
    /// it.
    ReadOnly,
    /// The backend does not support this operation at all.
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
            CortexError::NotEmpty => write!(f, "directory not empty"),
            CortexError::InvalidName => write!(f, "invalid name"),
            CortexError::InvalidArgument => write!(f, "invalid argument"),
            CortexError::FileTooLarge => write!(f, "file too large"),
            CortexError::BadHandle => write!(f, "bad file handle"),
            CortexError::PermissionDenied => write!(f, "permission denied"),
            CortexError::NoSpace => write!(f, "no space left on device"),
            CortexError::ReadOnly => write!(f, "read-only filesystem"),
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
            ErrorKind::DirectoryNotEmpty => CortexError::NotEmpty,
            ErrorKind::FileTooLarge => CortexError::FileTooLarge,
            // A backend that hands the whole request to the kernel in one call
            // (keeping `O_EXCL`/`O_TRUNC` atomic) has no pre-flight check to
            // report these from. Without these arms they reach callers as EIO.
            ErrorKind::IsADirectory => CortexError::IsADirectory,
            ErrorKind::NotADirectory => CortexError::NotADirectory,
            ErrorKind::InvalidInput => CortexError::InvalidArgument,
            // `find`/`rsync`/`tar` skip on EACCES but abort on EIO, so collapsing
            // the two loses a whole traversal to one unreadable file.
            ErrorKind::PermissionDenied => CortexError::PermissionDenied,
            ErrorKind::StorageFull => CortexError::NoSpace,
            ErrorKind::ReadOnlyFilesystem => CortexError::ReadOnly,
            _ => CortexError::Io(e),
        }
    }
}

pub type Result<T> = std::result::Result<T, CortexError>;
