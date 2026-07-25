use std::path::Path;

use crate::{Dirent, Result, Stat};

/// A logical, path-addressed store that can be exposed as a filesystem.
///
/// Unlike [`crate::Mountable`], this carries everything an adapter needs to
/// answer `lookup`/`getattr` — most importantly [`stat`](Self::stat), which
/// reports a single entry's metadata.
pub trait MountableV2: Send + Sync {
    /// Metadata for the entry at `path` (`NotFound` if it doesn't exist).
    fn stat(&self, path: &Path) -> Result<Stat>;

    /// The entries directly under `path`.
    fn list(&self, path: &Path) -> Result<Vec<Dirent>>;

    /// The full contents of the file at `path`.
    fn read(&self, path: &Path) -> Result<Vec<u8>>;
}
