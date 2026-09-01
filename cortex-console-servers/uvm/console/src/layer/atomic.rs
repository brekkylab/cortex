//! Putting a file somewhere so that nothing ever reads half of it.
//!
//! Write to a name nothing looks for, then rename. `rename(2)` within a filesystem is
//! atomic, so a reader sees the old file or the new one and never the middle of one being
//! written.
//!
//! # This is also the whole of the concurrency story
//!
//! Two processes putting the same layer both do the work, and the rename decides which copy
//! survives. It does not matter which: a layer is named by a digest of its input, so the
//! copies are identical. There is no inter-process locking anywhere in this workspace —
//! `assets::encode_erofs` already settles its races this way — and this is why none is
//! needed here either.

use std::path::{Path, PathBuf};

/// Write `bytes` to `path`, atomically.
pub fn replace(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| anyhow::anyhow!("making {}: {e}", parent.display()))?;

    // Beside the destination rather than in a temp directory, so the rename stays within one
    // filesystem — across one it is a copy, and a copy is not atomic.
    let temporary = unique(parent, "replace");
    std::fs::write(&temporary, bytes)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        anyhow::anyhow!("placing {}: {e}", path.display())
    })?;
    Ok(())
}

/// A path in `dir` that no other process or thread will pick.
///
/// The pid keeps two processes apart and the counter keeps two threads of one process apart.
/// Both matter: two consoles are two processes, and this crate's tests run in parallel in
/// one.
pub fn unique(dir: &Path, what: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{what}.{}.{n}.partial", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_file_holds_what_was_last_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thing");
        replace(&path, b"first").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        replace(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }

    #[test]
    fn nothing_is_left_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        replace(&dir.path().join("thing"), b"x").unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "temporaries were left behind: {names:?}");
    }

    #[test]
    fn a_missing_parent_is_made() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c/thing");
        replace(&path, b"x").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn two_temporaries_never_collide() {
        let dir = tempfile::tempdir().unwrap();
        let a = unique(dir.path(), "x");
        let b = unique(dir.path(), "x");
        assert_ne!(a, b);
    }
}
