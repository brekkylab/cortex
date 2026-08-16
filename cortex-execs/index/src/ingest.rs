//! `<name> ingest <path>...` — put files from the workspace into the index.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cortex::exec::{ExecCall, ExecResult};
use cortex::fs::Mount;

use crate::exec::host_path;
use crate::store::{Store, other};
use tantivy::schema::Value as _;

/// What a directory argument picks up. An allowlist and not a "skip what looks binary"
/// guess, because a wrong guess here is a document that silently is not searchable.
const INDEXED: &[&str] = &["md", "markdown", "txt", "rst"];

/// Index the files named by `args`; a directory means everything under it.
///
/// Re-ingesting a path **replaces** it rather than adding a second copy — see [`write_one`].
/// That is what makes running this twice mean the same as running it once, which is the only
/// version an agent can use without keeping track.
pub(crate) async fn run(
    store: &Arc<Store>,
    call: &ExecCall,
    mount: Option<&dyn Mount>,
    paths: &[String],
) -> ExecResult {
    // Resolved before anything is opened, so a mistyped argument is one refusal and not a
    // half-finished index. That `paths` is non-empty is the parser's to have enforced.
    let mut targets: Vec<(PathBuf, PathBuf)> = Vec::new();
    for arg in paths {
        let host = match host_path(call, mount, arg) {
            Ok(host) => host,
            Err(refusal) => return refusal,
        };
        let workspace = match call.resolve(arg) {
            Ok(path) => path,
            Err(e) => return ExecResult::failed(1, format!("{arg}: {e}\n")),
        };
        targets.push((workspace, host));
    }

    let store = store.clone();
    match tokio::task::spawn_blocking(move || index_all(&store, targets)).await {
        Ok(Ok(report)) => ExecResult::ok(report),
        Ok(Err(e)) => ExecResult::failed(1, format!("ingest: {e}\n")),
        // The blocking task panicked. Reported rather than resumed: the writer's lock is
        // released by the panic and the index may hold a partial batch.
        Err(e) => ExecResult::failed(1, format!("ingest: {e}\n")),
    }
}

/// The whole of the blocking half: walk, write, commit once.
///
/// One commit for the call and not one per file — a commit is an fsync and a segment, so
/// per-file would be both slow and a pile of tiny segments for the merge policy to clean up.
fn index_all(store: &Store, targets: Vec<(PathBuf, PathBuf)>) -> std::io::Result<String> {
    let mut files = Vec::new();
    for (workspace, host) in targets {
        collect(&workspace, &host, &mut files)?;
    }

    let mut written = 0usize;
    let mut skipped = Vec::new();
    {
        let mut writer = store.writer();
        for (workspace, host) in &files {
            match std::fs::read_to_string(host) {
                Ok(body) => {
                    write_one(store, &mut writer, workspace, &body)?;
                    written += 1;
                }
                // A file that is not UTF-8 is not a failure of the call: the allowlist said
                // what to look at, and this one turned out not to be text.
                Err(e) => skipped.push(format!("{}: {e}", workspace.display())),
            }
        }
        writer.commit().map_err(other)?;
    }

    let mut report = format!("indexed {written} file(s)\n");
    for line in &skipped {
        report.push_str(&format!("skipped {line}\n"));
    }
    Ok(report)
}

/// Replace the document for `workspace`, then add it.
///
/// `delete_term` before `add_document` in the same uncommitted batch is tantivy's idiom for
/// an upsert: the delete applies to everything already committed under that term, and the
/// add is what survives. Without it a second ingest of one path leaves two documents, and a
/// search answers the same file twice with different bodies.
fn write_one(
    store: &Store,
    writer: &mut tantivy::IndexWriter,
    workspace: &Path,
    body: &str,
) -> std::io::Result<()> {
    let fields = store.fields();
    let id = workspace.to_string_lossy().into_owned();
    let title = workspace
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| id.clone());

    writer.delete_term(tantivy::Term::from_field_text(fields.path, &id));

    let mut doc = tantivy::TantivyDocument::default();
    doc.add_text(fields.path, &id);
    doc.add_text(fields.title, &title);
    doc.add_text(fields.body, body);
    writer.add_document(doc).map_err(other)?;
    Ok(())
}

/// Every indexable file under `host`, paired with the workspace path it is known by.
///
/// Both paths are carried down together because only one of them can be walked and only the
/// other can be stored: the walk needs the host's directory entries, and a result has to
/// name a file the caller can open.
fn collect(
    workspace: &Path,
    host: &Path,
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> std::io::Result<()> {
    let meta = std::fs::metadata(host)?;
    if meta.is_file() {
        out.push((workspace.to_path_buf(), host.to_path_buf()));
        return Ok(());
    }
    if !meta.is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(host)? {
        let entry = entry?;
        let name = entry.file_name();
        // Dotfiles are skipped whole: `.git` is the case that matters, and walking into it
        // would index thousands of objects nobody asked about.
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let child_host = entry.path();
        let child_workspace = workspace.join(&name);

        if entry.file_type()?.is_dir() {
            collect(&child_workspace, &child_host, out)?;
        } else if indexable(&child_host) {
            out.push((child_workspace, child_host));
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

/// `<name> purge <STORE> <PATH>...` — take documents back out.
///
/// The inverse of [`run`], and **it takes no mount**: what is removed is decided by what the
/// index holds, not by what the tree does. That is not an omission — a file deleted from the
/// tree is the reason to purge in the first place, and requiring a mount would make the one
/// case this exists for the one case it could not serve.
///
/// A path removes the document at it and everything under it, so `purge notes /docs` undoes
/// `ingest notes /docs` whether that argument named a file or a directory.
pub(crate) async fn purge(store: &Arc<Store>, call: &ExecCall, paths: &[String]) -> ExecResult {
    let mut prefixes = Vec::new();
    for arg in paths {
        match call.resolve(arg) {
            Ok(path) => prefixes.push(path.to_string_lossy().into_owned()),
            Err(e) => return ExecResult::failed(1, format!("{arg}: {e}\n")),
        }
    }

    let store = store.clone();
    match tokio::task::spawn_blocking(move || purge_all(&store, &prefixes)).await {
        Ok(Ok(report)) => ExecResult::ok(report),
        Ok(Err(e)) => ExecResult::failed(1, format!("purge: {e}\n")),
        Err(e) => ExecResult::failed(1, format!("purge: {e}\n")),
    }
}

/// Every held path is read and matched here rather than asked of tantivy as a query.
///
/// `path` is a `STRING` field — one term, untokenized — so there is no prefix query to ask
/// for it, and the alternative would be a second field holding each parent just to make one
/// deletion expressible. Reading the paths costs a pass over the stored documents, which is
/// what a purge is worth: it is rare, and it is the operation whose job is to be exact.
fn purge_all(store: &Store, prefixes: &[String]) -> std::io::Result<String> {
    let fields = store.fields();
    let searcher = store.searcher()?;

    let held = searcher
        .search(
            &tantivy::query::AllQuery,
            &tantivy::collector::DocSetCollector,
        )
        .map_err(other)?;

    let mut doomed = Vec::new();
    for address in held {
        let doc: tantivy::TantivyDocument = searcher.doc(address).map_err(other)?;
        let Some(path) = doc.get_first(fields.path).and_then(|v| v.as_str()) else {
            continue;
        };
        if prefixes.iter().any(|prefix| under(path, prefix)) {
            doomed.push(path.to_owned());
        }
    }

    if doomed.is_empty() {
        return Ok("purged 0 document(s)\n".into());
    }

    let removed = doomed.len();
    {
        let mut writer = store.writer();
        for path in &doomed {
            writer.delete_term(tantivy::Term::from_field_text(fields.path, path));
        }
        writer.commit().map_err(other)?;
    }
    Ok(format!("purged {removed} document(s)\n"))
}

/// Whether `path` is `prefix` or lies under it.
///
/// Component-wise, which is the point: `notes-other/a.md` is not under `notes`, and a plain
/// `starts_with` would say it was.
fn under(path: &str, prefix: &str) -> bool {
    // The root, spelled as `resolve` spells it, covers everything.
    if prefix.is_empty() {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `<name> sync <STORE> <PATH>...` — make the index match the tree under `paths`.
///
/// [`run`] is idempotent for everything except a file that is *gone*: a second ingest picks
/// up what was added and replaces what changed, and leaves a document behind for what was
/// removed. This is that one case, and it is the composition of the two halves already here —
/// the walk from [`run`], the enumeration from [`purge`].
///
/// **A path that is not there means an empty tree under it**, so `sync` removes what the index
/// still holds for it. That is the reading a caller wants when a directory was deleted
/// wholesale; where nothing was indexed under the path anyway — a typo — the empty set meets
/// an empty set and nothing happens.
pub(crate) async fn sync(
    store: &Arc<Store>,
    call: &ExecCall,
    mount: Option<&dyn Mount>,
    paths: &[String],
) -> ExecResult {
    let mut targets = Vec::new();
    for arg in paths {
        let host = match host_path(call, mount, arg) {
            Ok(host) => host,
            Err(refusal) => return refusal,
        };
        let workspace = match call.resolve(arg) {
            Ok(path) => path,
            Err(e) => return ExecResult::failed(1, format!("{arg}: {e}\n")),
        };
        targets.push((workspace, host));
    }

    let store = store.clone();
    match tokio::task::spawn_blocking(move || sync_all(&store, targets)).await {
        Ok(Ok(report)) => ExecResult::ok(report),
        Ok(Err(e)) => ExecResult::failed(1, format!("sync: {e}\n")),
        Err(e) => ExecResult::failed(1, format!("sync: {e}\n")),
    }
}

/// One commit for both halves, so the index is never a tree that half-existed.
fn sync_all(store: &Store, targets: Vec<(PathBuf, PathBuf)>) -> std::io::Result<String> {
    // What the tree has under each path. A path that is gone contributes nothing rather than
    // failing the call — that is the case this exists for.
    let mut files = Vec::new();
    let mut prefixes = Vec::new();
    for (workspace, host) in &targets {
        prefixes.push(workspace.to_string_lossy().into_owned());
        match collect(workspace, host, &mut files) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    let walked: std::collections::BTreeSet<String> = files
        .iter()
        .map(|(workspace, _)| workspace.to_string_lossy().into_owned())
        .collect();

    // What the index holds under the same paths, and is no longer there to hold.
    let held = held_under(store, &prefixes)?;
    let doomed: Vec<&String> = held.iter().filter(|path| !walked.contains(*path)).collect();

    let fields = store.fields();
    let mut written = 0usize;
    let mut skipped = Vec::new();
    let removed = doomed.len();
    {
        let mut writer = store.writer();
        for path in doomed {
            writer.delete_term(tantivy::Term::from_field_text(fields.path, path));
        }
        for (workspace, host) in &files {
            match std::fs::read_to_string(host) {
                Ok(body) => {
                    write_one(store, &mut writer, workspace, &body)?;
                    written += 1;
                }
                Err(e) => skipped.push(format!("{}: {e}", workspace.display())),
            }
        }
        writer.commit().map_err(other)?;
    }

    let mut report = format!("synced {written} file(s), removed {removed} document(s)\n");
    for line in &skipped {
        report.push_str(&format!("skipped {line}\n"));
    }
    Ok(report)
}

/// Every indexed path lying under one of `prefixes`.
///
/// Shared by [`sync`] and [`purge`], which ask the same question of the index and differ only
/// in what they do with the answer.
fn held_under(store: &Store, prefixes: &[String]) -> std::io::Result<Vec<String>> {
    let fields = store.fields();
    let searcher = store.searcher()?;
    let held = searcher
        .search(
            &tantivy::query::AllQuery,
            &tantivy::collector::DocSetCollector,
        )
        .map_err(other)?;

    let mut found = Vec::new();
    for address in held {
        let doc: tantivy::TantivyDocument = searcher.doc(address).map_err(other)?;
        let Some(path) = doc.get_first(fields.path).and_then(|v| v.as_str()) else {
            continue;
        };
        if prefixes.iter().any(|prefix| under(path, prefix)) {
            found.push(path.to_owned());
        }
    }
    Ok(found)
}
