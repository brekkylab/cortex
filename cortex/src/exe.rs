//! Programs this crate — and a console server built on it — carries inside itself, written out
//! to be run.
//!
//! A program inside a binary cannot be `exec`'d; one in a file can. So a program that arrives
//! as bytes is written into a directory under a name its content decides, and run from there:
//! [`install`] is that, and what it hands back names the file.
//!
//! # Named by content
//!
//! `<stem>-<key>`, where the key changes whenever the bytes do. Two binaries carrying the same
//! program share one file; a newer one writes a file of its own beside it, and neither can
//! pick up the other's by accident. The file is written at a path nothing else will pick and
//! renamed into place, so one that exists is whole — two processes installing at once both do
//! the work and the rename decides which copy stays.
//!
//! # Taking the old ones away
//!
//! Content-named files accumulate: every upgrade leaves the previous one behind. So an install
//! also removes the other `<stem>-*` files in the directory — but **only ones nothing is about to
//! run**. Each install holds a shared `flock` on its file from finding it to starting it, and a
//! file is removed only by whoever takes an exclusive lock on it without waiting.
//!
//! Only that window needs guarding, which is why the lock is held by the caller for as long as
//! it keeps the [`Installed`] and not for the length of whatever it starts: a running program
//! keeps its own file alive on every unix whatever happens to its name. The one place removal
//! could hurt is between finding the file and `exec`ing it, and an install that finds its file
//! gone once it holds the lock — it lost that race — writes it again.
//!
//! Losing costs one rewrite and nothing else, because whoever installs a program has its bytes:
//! a program removed from under a binary that still carries it is installed again by the next
//! start of that binary.
//!
//! Not part of this crate's API — it is public so that a console server can install what *it*
//! carries the same way, and is hidden from the documentation for the same reason.

use std::{
    fs::File,
    io,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// How many times an install that keeps losing its file to a removal tries again before it
/// says so. Losing once is already a race between two processes on one machine; losing this
/// many times in a row is something else.
const ATTEMPTS: usize = 5;

/// How old a half-written file has to be before it is taken for a crash's rather than an
/// install in progress. Writing one is a second or two; this is a margin, not a measurement.
const ABANDONED_AFTER: Duration = Duration::from_secs(60 * 60);

/// A program written out, and held where it is for as long as this is.
///
/// Start it while this is alive; drop it once it has started. See the module documentation for
/// why that is the whole of what needs holding.
#[derive(Debug)]
pub struct Installed {
    path: PathBuf,
    /// The shared lock that keeps a removal off the file. Released on drop.
    _held: File,
}

impl Installed {
    /// The file to run.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Write a program into `dir` as `<stem>-<key>` unless it is there already, and hold it.
///
/// `fill` writes the program to the path it is given, which is a temporary one in `dir`; it is
/// made executable and renamed into place afterwards. It can be called more than once — see
/// the module documentation for when — and is not called at all when the file already exists.
///
/// `key` is the caller's, and has to change whenever the bytes do: a content hash, usually.
/// Every other `<stem>-*` file in `dir` that nothing holds is removed on the way.
pub fn install(
    dir: &Path,
    stem: &str,
    key: &str,
    mut fill: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<Installed> {
    std::fs::create_dir_all(dir)?;
    let name = format!("{stem}-{key}");
    let path = dir.join(&name);

    for _ in 0..ATTEMPTS {
        if !path.exists() {
            let tmp = dir.join(format!("{name}.{}.{}.tmp", std::process::id(), seq()));
            let written = fill(&tmp).and_then(|()| {
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
                std::fs::rename(&tmp, &path)
            });
            if let Err(e) = written {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }

        // Opened by name and then checked against the name: a removal between the two leaves a
        // handle on a file nothing can `exec` any more, and that is the race to go round again
        // for.
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        flock(&file, libc::LOCK_SH)?;
        let held = file.metadata()?;
        match std::fs::metadata(&path) {
            Ok(named) if named.dev() == held.dev() && named.ino() == held.ino() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }

        prune(dir, stem, &name);
        return Ok(Installed { path, _held: file });
    }

    Err(io::Error::other(format!(
        "{} kept being removed while it was being installed",
        path.display()
    )))
}

/// Remove every other `<stem>-*` in `dir` that nothing holds, and every half-written one old
/// enough to have been abandoned.
///
/// Best effort, and silent: a file that cannot be removed is one somebody else is using or
/// owns, and what it costs is space rather than correctness.
fn prune(dir: &Path, stem: &str, keep: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{stem}-");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == keep || !name.starts_with(&prefix) {
            continue;
        }

        let path = entry.path();
        if name.ends_with(".tmp") {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age > ABANDONED_AFTER);
            if old {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }

        let Ok(file) = File::open(&path) else {
            continue;
        };
        if flock(&file, libc::LOCK_EX | libc::LOCK_NB).is_ok() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn flock(file: &File, operation: libc::c_int) -> io::Result<()> {
    // SAFETY: `flock` takes a descriptor and an operation, and the descriptor is `file`'s for as
    // long as this borrow is.
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Distinguishes two installs in one process, which share a pid.
fn seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writes(bytes: &'static [u8]) -> impl FnMut(&Path) -> io::Result<()> {
        move |path| std::fs::write(path, bytes)
    }

    #[test]
    fn an_install_is_written_once_and_found_after() {
        let dir = tempfile::tempdir().unwrap();
        let mut calls = 0;
        let first = install(dir.path(), "prog", "aaaa", |path| {
            calls += 1;
            std::fs::write(path, b"#!/bin/sh\n")
        })
        .unwrap();
        assert_eq!(first.path(), dir.path().join("prog-aaaa"));
        let mode = std::fs::metadata(first.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        drop(first);

        let again = install(dir.path(), "prog", "aaaa", |_| {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 1, "a file that is there is not written again");
        assert_eq!(std::fs::read(again.path()).unwrap(), b"#!/bin/sh\n");
    }

    #[test]
    fn an_install_takes_the_old_ones_away_unless_something_holds_them() {
        let dir = tempfile::tempdir().unwrap();
        let held = install(dir.path(), "prog", "held", writes(b"1")).unwrap();
        drop(install(dir.path(), "prog", "free", writes(b"2")).unwrap());
        // Another program's files are not this one's to remove.
        std::fs::write(dir.path().join("other-free"), b"3").unwrap();

        let _new = install(dir.path(), "prog", "new", writes(b"4")).unwrap();
        assert!(held.path().exists(), "a file somebody holds stays");
        assert!(
            !dir.path().join("prog-free").exists(),
            "one nobody holds goes"
        );
        assert!(dir.path().join("other-free").exists());
    }

    #[test]
    fn a_failed_write_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let e = install(dir.path(), "prog", "k", |path| {
            std::fs::write(path, b"half")?;
            Err(io::Error::other("no"))
        })
        .unwrap_err();
        assert_eq!(e.to_string(), "no");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
