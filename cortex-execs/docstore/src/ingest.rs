//! `docstore ingest|sync|purge` — what goes into the store, and what comes back out.
//!
//! Which files a directory argument picks up, how large one may be, what a dangling symlink
//! counts as, and what a second `ingest` of a path means are all decided here, and none of them
//! have anything to do with the store underneath: the walk hands over text and a stamp, and what
//! becomes of those is [`store`](crate::store)'s.

use std::path::{Component, Path, PathBuf};

use crate::store::{Held, Store, Writing};

/// What a directory argument picks up. An allowlist and not a "skip what looks binary"
/// guess, because a wrong guess here is a document that silently is not searchable.
const INDEXED: &[&str] = &["md", "markdown", "txt", "rst"];

/// The largest file this will read into a store.
///
/// A body is held in memory whole and then written, so one file can cost several times its own
/// size. 8 MiB is far past any prose and well short of what a stray database dump or minified
/// bundle would be, and a file over it is named in the report rather than skipped in silence.
const MAX_BODY: u64 = 8 * 1024 * 1024;

/// What a file was, cheaply: when it changed and how big it was.
///
/// Enough to decide whether the store already has its bytes, and no more. mtime alone would
/// miss an edit made within one timestamp tick, which some filesystems round to a second, so
/// the length rides along.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Stamp {
    pub mtime: u64,
    pub len: u64,
}

impl Stamp {
    /// Whether the store already holds this file's bytes.
    ///
    /// `0` is "no mtime", which some filesystems and some errors give, and it must never read
    /// as a match: two files that both failed to report one would otherwise look identical
    /// forever. Equality alone would say they do.
    fn unchanged_from(&self, current: &Stamp) -> bool {
        self.mtime != 0 && current.mtime != 0 && self == current
    }

    /// Nanoseconds since the epoch, saturating. A clock before 1970 is a machine this has
    /// nothing useful to say about, and `0` makes every such file look changed, which is the
    /// safe direction.
    fn of(meta: &std::fs::Metadata) -> Stamp {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
            .unwrap_or(0);
        Stamp {
            mtime,
            len: meta.len(),
        }
    }
}

/// `path` with the components that name nothing taken out, and nothing else touched.
///
/// This is what a document is filed under, and it is also what gets opened, which is the whole
/// reason it does so little. `..` stays, and so does a leading `/`: resolving either would mean
/// deciding on the caller's behalf what file they meant, and the kernel resolving the same path
/// afterwards could disagree — a store asserting the contents of a file that was never opened is
/// the failure this avoids. Taking `.` out is safe because no `.` names a different place.
///
/// The result of `.` is the empty path, which is this program's spelling for "where it was run",
/// and what makes a document under it `docs/a.md` rather than `./docs/a.md`. [`readable`] is what
/// turns it back into something to open.
fn cleaned(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect()
}

/// A path as something to open: the empty path is where this was run, which is `.` to the
/// filesystem and nothing at all to a document's name.
fn readable(path: &Path) -> &Path {
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}

/// Index the files named by `paths`; a directory means everything under it.
///
/// Re-ingesting a path **replaces** it rather than adding a second copy — `path` is the
/// document's identity in the schema, and the upsert is keyed on it. That is what makes running
/// this twice mean the same as running it once, which is the only version an agent can use
/// without keeping track.
pub(crate) fn run(store: &Store, paths: &[PathBuf]) -> std::io::Result<String> {
    let mut walk = Walk::default();
    for path in paths {
        collect(&cleaned(path), &mut walk)?;
    }
    walk.dedup();

    let mut skipped = walk.declined;
    let mut writing = Vec::new();
    for file in &walk.files {
        match std::fs::read_to_string(&file.path) {
            Ok(body) => writing.push(Writing {
                path: &file.path,
                title: title_of(&file.path),
                body,
                stamp: file.stamp,
            }),
            // A file that is not UTF-8 is not a failure of the call: the allowlist said what to
            // look at, and this one turned out not to be text.
            Err(e) => skipped.push(format!("{}: {e}", file.path.display())),
        }
    }

    let written = writing.len();
    store.apply(&writing, &[])?;

    let mut report = format!("indexed {written} file(s)\n");
    for line in &skipped {
        report.push_str(&format!("skipped {line}\n"));
    }
    Ok(report)
}

/// `docstore sync <STORE> <PATH>...` — make the store match the tree under `paths`.
///
/// [`run`] is idempotent for everything except a file that is *gone*: a second ingest picks up
/// what was added and replaces what changed, and leaves a document behind for what was removed.
/// This is that one case.
///
/// **A path that is not there means an empty tree under it**, so `sync` removes what the store
/// still holds for it. That is the reading a caller wants when a directory was deleted wholesale;
/// where nothing was indexed under the path anyway — a typo — the empty set meets an empty set
/// and nothing happens.
pub(crate) fn sync(store: &Store, paths: &[PathBuf], force: bool) -> std::io::Result<String> {
    let mut walk = Walk::default();
    let mut prefixes = Vec::new();
    for path in paths {
        let path = cleaned(path);
        prefixes.push(path.to_string_lossy().into_owned());
        // Only *this* path being absent means an empty tree under it. A `NotFound` from
        // somewhere inside the walk is a file that went while it was being read, and taking
        // that for "the whole path is gone" would delete everything the walk had not reached
        // yet.
        if !readable(&path).exists() {
            continue;
        }
        collect(&path, &mut walk)?;
    }
    walk.dedup();

    let held: Vec<Held> = store
        .held()?
        .into_iter()
        .filter(|held| prefixes.iter().any(|prefix| under(&held.path, prefix)))
        .collect();
    let by_path: std::collections::BTreeMap<&str, &Held> =
        held.iter().map(|h| (h.path.as_str(), h)).collect();

    // Three groups, decided without opening a single file: what the store has never seen, what
    // it has under a different stamp, and what it already holds the bytes of.
    //
    // `doomed` starts as everything held under these paths and loses each path the walk found,
    // so what is left is exactly what the walk did not produce — whether it never reached the
    // path or reached it and declined it, since a declined file is not among `walk.files`
    // either. That covers both halves in one pass: a file the walk never reached and a file it
    // reached and declined are alike in producing no entry, and neither is removed from `doomed`.
    let mut doomed: std::collections::BTreeMap<&str, i64> =
        by_path.iter().map(|(path, h)| (*path, h.rowid)).collect();

    let walked: Vec<String> = walk
        .files
        .iter()
        .map(|f| f.path.to_string_lossy().into_owned())
        .collect();

    let mut to_read = Vec::new();
    let mut unchanged = 0usize;
    for (file, path) in walk.files.iter().zip(&walked) {
        match by_path.get(path.as_str()) {
            Some(held) if held.stamp.unchanged_from(&file.stamp) && !force => unchanged += 1,
            _ => to_read.push((file, path)),
        }
        doomed.remove(path.as_str());
    }

    let mut skipped = walk.declined;
    let mut writing = Vec::new();
    for (file, path) in to_read {
        match std::fs::read_to_string(&file.path) {
            Ok(body) => writing.push(Writing {
                path: &file.path,
                title: title_of(&file.path),
                body,
                stamp: file.stamp,
            }),
            // Read at the walk, not readable at the read: the same rule as everything else this
            // declines. A document the store holds for it would be the contents of a file that
            // cannot be opened, so it goes back onto the list.
            Err(e) => {
                skipped.push(format!("{path}: {e}"));
                if let Some((held_path, held)) = by_path.get_key_value(path.as_str()) {
                    doomed.insert(held_path, held.rowid);
                }
            }
        }
    }

    let written = writing.len();
    let removed: Vec<i64> = doomed.values().copied().collect();
    // One transaction for both halves, so the store is never a tree that half-existed.
    store.apply(&writing, &removed)?;

    let mut report = format!(
        "synced {written} file(s), {unchanged} unchanged, removed {} document(s)\n",
        removed.len()
    );
    for line in &skipped {
        report.push_str(&format!("skipped {line}\n"));
    }
    Ok(report)
}

/// `docstore purge <STORE> <PATH>...` — take documents back out.
///
/// The inverse of [`run`], and it opens no file in the tree: what is removed is decided by what
/// the store holds, not by what the tree does. A file deleted from the tree is the reason to
/// purge in the first place.
///
/// A path removes the document at it and everything under it, so `purge notes.db docs` undoes
/// `ingest notes.db docs` whether that argument named a file or a directory.
pub(crate) fn purge(store: &Store, paths: &[PathBuf]) -> std::io::Result<String> {
    let prefixes: Vec<String> = paths
        .iter()
        .map(|path| cleaned(path).to_string_lossy().into_owned())
        .collect();

    let doomed: Vec<i64> = store
        .held()?
        .into_iter()
        .filter(|held| prefixes.iter().any(|prefix| under(&held.path, prefix)))
        .map(|held| held.rowid)
        .collect();
    store.apply(&[], &doomed)?;
    Ok(format!("purged {} document(s)\n", doomed.len()))
}

/// The name a document is also searchable by, so a query matching a file's name outranks one
/// matching a mention in somebody else's body.
fn title_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// One file the walk found: what it is filed under, and what it was when it was looked at.
pub(crate) struct Walked {
    pub path: PathBuf,
    pub stamp: Stamp,
}

/// What a walk came back with.
///
/// **A file this declines to read is an absent file**, and that is one rule rather than a case
/// per reason. A dangling symlink, a file too large to hold, one whose bytes turn out not to be
/// text: each ends with no document written, so each must end with no document *held* —
/// otherwise the store goes on asserting the contents of a file nothing will open again.
/// `declined` is what says so out loud, since silence and success read the same to a caller.
#[derive(Default)]
pub(crate) struct Walk {
    pub files: Vec<Walked>,
    pub declined: Vec<String>,
}

impl Walk {
    /// Admit a file, or decline it for a reason a caller can act on.
    ///
    /// The size cap lives here and not at the read, so that "too large" and "not there" leave
    /// the walk by the same door: both are files this will not have a document for.
    fn take(&mut self, path: &Path, meta: &std::fs::Metadata) {
        if meta.len() > MAX_BODY {
            self.declined.push(format!(
                "{}: {} bytes, over the {MAX_BODY} this reads",
                path.display(),
                meta.len()
            ));
            return;
        }
        self.files.push(Walked {
            path: path.to_path_buf(),
            stamp: Stamp::of(meta),
        });
    }

    /// Drop what two overlapping arguments found twice.
    ///
    /// `ingest notes.db a a/b` walks `a/b` under both, and without this the same file is written
    /// twice in one batch and counted twice in the report. The documents come out right either
    /// way, because a write is an upsert; the count does not.
    fn dedup(&mut self) {
        let mut seen = std::collections::BTreeSet::new();
        self.files
            .retain(|f| seen.insert(f.path.to_string_lossy().into_owned()));
        self.declined.sort();
        self.declined.dedup();
    }
}

/// Every indexable file under `path`, filed under the path it is reached by.
fn collect(path: &Path, out: &mut Walk) -> std::io::Result<()> {
    let meta = std::fs::metadata(readable(path))?;
    if meta.is_file() {
        out.take(path, &meta);
        return Ok(());
    }
    if !meta.is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(readable(path))? {
        let entry = entry?;
        let name = entry.file_name();
        // Dotfiles are skipped whole: `.git` is the case that matters, and walking into it
        // would index thousands of objects nobody asked about.
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        // Joined onto the path this was reached by rather than taken from the directory entry,
        // so that a document's name is the caller's spelling all the way down: under `docs` the
        // answer is `docs/sub/a.md`, and under `.` it is `sub/a.md`.
        let child = path.join(&name);

        if entry.file_type()?.is_dir() {
            collect(&child, out)?;
        } else if indexable(&child) {
            // `metadata` and not `entry.metadata()`: a symlink should be measured by what it
            // points at, which is also what will be read. One that points nowhere fails here and
            // is left out of the walk entirely, so `sync` treats it as the absent file it is
            // rather than as a file it merely could not read.
            match std::fs::metadata(&child) {
                Ok(meta) => out.take(&child, &meta),
                // Not there at all: a dangling symlink, or a file that went while this was
                // walking. Absent, and so not among what the tree has.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // There but not readable. Declined rather than fatal: one file nobody can open
                // should not end a walk over everything beside it.
                Err(e) => out.declined.push(format!("{}: {e}", child.display())),
            }
        }
    }
    Ok(())
}

fn indexable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| INDEXED.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Whether `path` is `prefix` or lies under it.
///
/// Component-wise, which is the point: `notes-other/a.md` is not under `notes`, and a plain
/// `starts_with` would say it was. It is also why the store hands back every document and this
/// does the filtering — a SQL `like` would have to escape `%` and `_` out of every path, and
/// would be a second spelling of this rule.
pub(crate) fn under(path: &str, prefix: &str) -> bool {
    // The empty path is where this was run, and everything is under that.
    if prefix.is_empty() {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}
