//! Entry metadata: [`Stat`] and [`DirentKind`].

use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirentKind {
    File,
    Dir,
}

/// Metadata about a single directory entry.
///
/// Fields beyond `kind`/`size` are optional because different backends expose
/// different subsets (local files, S3 objects, Notion pages, …).
#[derive(Clone, Debug)]
pub struct Stat {
    pub kind: DirentKind,

    pub size: u64,

    /// Last-modified time, if the backend reports one (S3 `LastModified`,
    /// Notion `last_edited_time`).
    pub mtime: Option<SystemTime>,

    /// Last-access time, if the backend reports one. Most object/document
    /// backends don't track access time, so this is usually `None`.
    pub atime: Option<SystemTime>,

    /// Change/creation time, if the backend reports one (Notion `created_time`).
    /// Not POSIX `ctime` exactly — the nearest timestamp the backend exposes.
    pub ctime: Option<SystemTime>,

    /// Birth/creation time, if the backend reports one (local files' `created`);
    /// `None` for providers that don't distinguish a birth time.
    pub created: Option<SystemTime>,

    /// Entity tag / content fingerprint, if available (S3 `ETag`).
    pub etag: Option<String>,

    /// Version id, if the backend is versioned (S3 `VersionId`).
    pub version: Option<String>,
}

impl Stat {
    /// Convenience constructor for a stat with only `kind` and `size` set and
    /// every optional field left as `None`.
    pub fn new(kind: DirentKind, size: u64) -> Self {
        Self {
            kind,
            size,
            mtime: None,
            atime: None,
            ctime: None,
            created: None,
            etag: None,
            version: None,
        }
    }
}
