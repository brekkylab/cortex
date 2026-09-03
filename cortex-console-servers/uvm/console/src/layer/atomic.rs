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
//!
//! # What a rename cannot cover
//!
//! `SIGKILL`. A console killed while writing a layer leaves a temporary the size of that
//! layer in the store, under a name nothing looks for, and no sweep elsewhere in this
//! workspace goes near the store — `assets::sweep_abandoned` walks the temp directories and
//! matches its own prefix. So [`sweep`] is here, and the pid in the name is what lets it
//! tell an abandoned temporary from one being written right now.

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
/// one. The pid is also what [`sweep`] reads back.
pub fn unique(dir: &Path, what: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{PREFIX}{what}.{}.{n}{SUFFIX}", std::process::id()))
}

/// What a temporary is called, either side of the `<what>.<pid>.<n>` in the middle.
const PREFIX: &str = ".";
const SUFFIX: &str = ".partial";

/// Delete the temporaries in `dir` that belong to processes which are gone.
///
/// Best effort by construction: a temporary whose owner is still running is left alone, and
/// so is anything this cannot read or remove. It says nothing and answers nothing — a store
/// that could not be tidied is still a store that works.
pub fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(owner) else {
            continue;
        };
        if pid == std::process::id() || !is_gone(pid) {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

/// The pid out of a temporary's name, or `None` for a name that is not one.
///
/// Read from the right, because `<what>` is a caller's word and may hold dots — a layer's
/// file stem does not, but nothing in the signature of [`unique`] says so.
fn owner(name: &str) -> Option<u32> {
    let middle = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    let (rest, _counter) = middle.rsplit_once('.')?;
    let (what, pid) = rest.rsplit_once('.')?;
    (!what.is_empty()).then_some(())?;
    pid.parse().ok()
}

/// Whether the process that owned a temporary has exited.
///
/// A pid that exists but has since been handed to somebody else answers "not gone", which is
/// the safe way round: what an extra temporary costs is disk, and what deleting a live one
/// costs is a layer that fails to be written.
fn is_gone(pid: u32) -> bool {
    // Only a real pid reaches `kill`. Zero and the negatives it would be read as are that
    // call's way of naming a *group* of processes — `kill(-1, …)` is every process this user
    // has — and a name that came out of a file must never be able to ask that question.
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs only the permission/existence check; it cannot
    // affect this or any other process.
    let answer = unsafe { libc::kill(pid, 0) };
    answer == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
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

    #[test]
    fn a_temporary_names_the_process_that_made_it() {
        let dir = tempfile::tempdir().unwrap();
        // A `what` with a dot in it, because reading the pid from the left would find one.
        let path = unique(dir.path(), "abc.def");
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            owner(name),
            Some(std::process::id()),
            "{name} does not say who made it"
        );
    }

    #[test]
    fn nothing_that_is_not_a_temporary_is_read_as_one() {
        for bad in ["layer.erofs", "layer.map", ".partial", ".a.b.partial", "x"] {
            assert!(owner(bad).is_none(), "{bad:?} was read as a temporary");
        }
    }

    /// A name is a name and not an argument to `kill(2)`: the values that would ask about a
    /// whole process group have to answer "still running" rather than reaching the call.
    #[test]
    fn a_pid_that_names_no_process_is_never_read_as_a_dead_one() {
        for pid in [0, u32::MAX, i32::MAX as u32 + 1] {
            assert!(
                !is_gone(pid),
                "{pid} was taken for a process that had exited"
            );
        }
    }

    /// The case the sweep exists for: a console killed part-way through writing a layer.
    #[test]
    fn a_temporary_a_dead_process_left_is_swept() {
        let dir = tempfile::tempdir().unwrap();
        // The largest a pid can be spelled as, which is far above any a kernel hands out —
        // so `kill(2)` answers ESRCH and this stays a test about the sweep rather than a
        // test about which pids happen to be live right now.
        let abandoned = dir
            .path()
            .join(format!("{PREFIX}layer.{}.0{SUFFIX}", libc::pid_t::MAX));
        std::fs::write(&abandoned, b"half a layer").unwrap();
        let ours = unique(dir.path(), "layer");
        std::fs::write(&ours, b"being written").unwrap();
        let real = dir.path().join("layer.erofs");
        std::fs::write(&real, b"a layer").unwrap();

        sweep(dir.path());

        assert!(!abandoned.exists(), "the abandoned temporary survived");
        assert!(ours.exists(), "a temporary being written now was swept");
        assert!(real.exists(), "a real file was swept");
    }
}
