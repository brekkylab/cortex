//! `<name> <init|ingest|search|sync|purge|list|drop> ...` — the parser, and the branch on what
//! it produced.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use cortex::BoxFuture;
use cortex::exec::{ExecCall, ExecResult, Executable};
use cortex::fs::Mount;
use cortex_exec_storebase::command::{as_registered, host_path, rendered_by_clap};

use crate::search::DEFAULT_LIMIT;
use crate::store::Store;
use crate::{ingest, search};

/// Full-text search over files in the tree a session is working in, kept in a store that is
/// itself a file in that tree.
///
/// # A store is a path, not a name
///
/// A store is **one file of the tree**, named the way any other file is. That is possible
/// because SQLite on the rollback journal needs nothing of its host but the ability to create a
/// sibling file: an engine that `mmap`ed its own data or held a lock file across a session would
/// have to keep that state off the tree, since under FUSE-T a mount is an NFS one and both are
/// exactly the operations that behave differently there.
///
/// That decides several things at once. `list` and `drop` still exist but ask about a
/// directory rather than about a root; there is no `<STORE>` name to validate, because
/// [`ExecCall::resolve`] already refuses anything that would leave the workspace; and a store
/// can be copied, committed or read with `sqlite3` like any other document.
///
/// It also costs one thing. Every command needs the mount, including `search` and `purge`,
/// which did not before — a store that is a path cannot be reached without a tree to resolve
/// the path against.
#[derive(Debug, Clone, Default)]
pub struct DocStore {}

impl DocStore {
    /// What [`register`](cortex::exec::ExecutableSet::register) wants: one line for a list,
    /// not usage.
    pub const SUMMARY: &'static str = "index files from the workspace and search them";

    pub fn new() -> Self {
        Self {}
    }
}

// The parse of one call.
//
// A real parser and not a `match` on the first argument, because `--help` has to work on every
// subcommand and not only on the name — which is the whole of what an agent has to go on once
// it has decided to call this. Nothing here intercepts an argument: a registered name is a
// command line like any other, and a command line answers its own help.
//
// `//` and not `///` throughout this file's clap types: a doc comment here is what `--help`
// prints, so anything a *caller* would not act on belongs in a comment instead.
#[derive(Parser)]
#[command(
    about = DocStore::SUMMARY,
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "<STORE> is a path in this tree, like any other argument. `init` makes one; every other command expects it to be there already.",
    // No `--version`: a registered name is not a package, and the version a caller could act
    // on is the consumer's, not this executable's.
    disable_version_flag = true,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Make a store where there is no file yet.
    ///
    /// The one command that brings a store into being. Every other one expects the file to be
    /// there, which is what keeps a mistyped path from becoming an empty store that answers
    /// every search with nothing.
    Init {
        /// Where to make it — a path in this tree, `/work/notes.db`.
        #[arg(value_name = "STORE")]
        store: String,
    },

    /// Index files from the workspace into a store.
    ///
    /// A directory means everything under it. Re-ingesting a path replaces it, so running this
    /// twice means the same as running it once.
    Ingest {
        /// Which store to write.
        #[arg(value_name = "STORE")]
        store: String,

        /// Workspace paths — `/notes`, `/notes/a.md`. Relative paths are refused while the
        /// backend reports no working directory.
        //
        // `String` and not `PathBuf`: a path here is workspace-relative and only
        // `ExecCall::resolve` can turn it into one, so parsing it as a host path would be the
        // wrong answer arrived at early.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
    },

    /// Search a store.
    Search {
        /// Which store to ask.
        #[arg(value_name = "STORE")]
        store: String,

        /// How many hits to answer with.
        #[arg(short = 'n', long = "limit", value_name = "N", default_value_t = DEFAULT_LIMIT)]
        limit: usize,

        /// The words to look for, joined — a shell has already split them.
        #[arg(required = true, value_name = "QUERY")]
        query: Vec<String>,
    },

    /// Make a store match the tree under the paths given.
    ///
    /// What a second `ingest` cannot do: it picks up what was added and replaces what changed,
    /// but leaves a document behind for a file that is gone. This removes those too, so the
    /// store says what the tree says.
    ///
    /// Only files whose size or modification time moved are read again.
    Sync {
        /// Which store to bring up to date.
        #[arg(value_name = "STORE")]
        store: String,

        /// Workspace paths, spelled as `ingest` spelled them. A path that is no longer there
        /// means everything the store held under it goes.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,

        /// Re-read every file, not only the ones whose size or modification time moved.
        ///
        /// What to reach for when a file was edited within one timestamp tick and kept its
        /// length, which some filesystems round to a second.
        #[arg(long)]
        force: bool,
    },

    /// Take documents back out of a store, by the paths they were ingested under.
    ///
    /// A path removes the document at it and everything under it. Opens no file in the tree: a
    /// file gone from the tree is the reason to do this, and it is still in the store.
    Purge {
        /// Which store to take them out of.
        #[arg(value_name = "STORE")]
        store: String,

        /// Workspace paths, spelled as `ingest` spelled them.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
    },

    /// The stores in a directory, and how much is in each.
    ///
    /// Looks at that one directory and does not descend into it. Files that are not stores are
    /// passed over in silence, since a directory holding other things is the normal case.
    List {
        /// Which directory to look in. The calling command's own, if it is left out.
        #[arg(value_name = "DIR")]
        dir: Option<String>,
    },

    /// Remove a store. The files it was built from are untouched.
    ///
    /// Refuses anything that is not a store, which is the whole of what this does that `rm`
    /// does not: a store is a path, so one mistyped argument could otherwise be somebody's
    /// data.
    Drop {
        /// Which store to forget.
        #[arg(value_name = "STORE")]
        store: String,
    },
}

impl Executable for DocStore {
    /// Every command needs the mount, because every command names a store and a store is a path
    /// in the tree — see the type's docs. `search` and `purge` still open no *indexed* file:
    /// what they need the mount for is the store itself.
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecResult> {
        Box::pin(async move {
            let parsed = match parse(call) {
                Ok(parsed) => parsed,
                Err(answer) => return answer,
            };

            match parsed.command {
                Command::Init { store } => init(call, mount, &store).await,

                Command::Ingest { store, paths } => match opened(call, mount, &store).await {
                    Ok(opened) => ingest::run(opened, call, mount, &paths).await,
                    Err(refusal) => refusal,
                },

                Command::Search {
                    store,
                    limit,
                    query,
                } => match opened(call, mount, &store).await {
                    Ok(opened) => search::run(opened, limit, &query.join(" ")).await,
                    Err(refusal) => refusal,
                },

                Command::Sync {
                    store,
                    paths,
                    force,
                } => match opened(call, mount, &store).await {
                    Ok(opened) => ingest::sync(opened, call, mount, &paths, force).await,
                    Err(refusal) => refusal,
                },

                Command::Purge { store, paths } => match opened(call, mount, &store).await {
                    Ok(opened) => ingest::purge(opened, call, &paths).await,
                    Err(refusal) => refusal,
                },

                Command::List { dir } => list(call, mount, dir.as_deref()).await,

                Command::Drop { store } => drop_store(call, mount, &store).await,
            }
        })
    }
}

/// `init` — the file, and its schema.
async fn init(call: &ExecCall, mount: Option<&dyn Mount>, store: &str) -> ExecResult {
    let host = match host_path(call, mount, store) {
        Ok(host) => host,
        Err(refusal) => return refusal,
    };
    let named = store.to_owned();
    let name = call.name.clone();

    match tokio::task::spawn_blocking(move || Store::try_new(&host)).await {
        // The store, named as it was asked for, and nothing else: a line a script can read as
        // the path it now has. What went right needs no sentence — the file is the answer.
        Ok(Ok(_)) => ExecResult::ok(format!("{named}\n")),
        // The one failure worth a sentence of its own. `File exists` is true and says nothing
        // about what to do, where the caller is either looking at a store they already have —
        // and wanted the command that writes to one — or at a path they did not mean to type.
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => ExecResult::failed(
            1,
            format!(
                "{name}: {named}: there is already a file here; \
                 `{name} ingest` writes to a store that exists\n"
            ),
        ),
        Ok(Err(e)) => ExecResult::failed(1, format!("{name}: {named}: {e}\n")),
        Err(e) => ExecResult::failed(1, format!("{name}: {named}: {e}\n")),
    }
}

/// The store `arg` names, opened — or the refusal to answer with.
///
/// Named as the caller named it: the host path is on the far side of a mount they cannot see,
/// and would not tell them which store was meant.
async fn opened(
    call: &ExecCall,
    mount: Option<&dyn Mount>,
    arg: &str,
) -> Result<Arc<Store>, ExecResult> {
    let host = host_path(call, mount, arg)?;
    let named = arg.to_owned();
    let name = call.name.clone();

    match tokio::task::spawn_blocking(move || Store::try_from_file(&host)).await {
        Ok(Ok(store)) => Ok(Arc::new(store)),
        Ok(Err(e)) => Err(ExecResult::failed(1, format!("{name}: {named}: {e}\n"))),
        Err(e) => Err(ExecResult::failed(1, format!("{name}: {named}: {e}\n"))),
    }
}

/// `list` — the stores in one directory, with the number of documents in each.
async fn list(call: &ExecCall, mount: Option<&dyn Mount>, dir: Option<&str>) -> ExecResult {
    // `.` when nothing was named, which resolves against wherever the calling command stood —
    // and is refused, like any relative path, where the backend reports no working directory.
    let arg = dir.unwrap_or(".");
    let host = match host_path(call, mount, arg) {
        Ok(host) => host,
        Err(refusal) => return refusal,
    };
    let name = call.name.clone();

    let found = tokio::task::spawn_blocking(move || stores_in(&host)).await;
    let (listed, refused) = match found {
        Ok(Ok(found)) => found,
        Ok(Err(e)) => return ExecResult::failed(1, format!("{name}: {arg}: {e}\n")),
        Err(e) => return ExecResult::failed(1, format!("{name}: {arg}: {e}\n")),
    };

    if listed.is_empty() && refused.is_empty() {
        return ExecResult::ok(format!(
            "no stores here — `{name} init <STORE>` makes one\n"
        ));
    }

    let mut out = String::new();
    for (store, docs) in &listed {
        match docs {
            Some(docs) => out.push_str(&format!("{store}\t{docs} document(s)\n")),
            None => out.push_str(&format!("{store}\t(unreadable)\n")),
        }
    }

    if refused.is_empty() {
        return ExecResult::ok(out);
    }
    // A store that will not open is still a store, and naming it beside the others is more use
    // than failing the whole listing over one of them. The exit code still says something went
    // wrong, because a caller asking what it has should not read a broken store as an answer.
    ExecResult {
        stdout: out.into_bytes(),
        stderr: format!("{}\n", refused.join("\n")).into_bytes(),
        exit_code: 1,
        timed_out: false,
    }
}

/// Every store directly in `host`, with its document count, and the ones that would not open.
///
/// Sorted by name, and not recursive: `list` answers about a directory, and a walk would turn a
/// question about what is here into a question about everything below.
#[allow(clippy::type_complexity)]
fn stores_in(host: &std::path::Path) -> std::io::Result<(Vec<(String, Option<u64>)>, Vec<String>)> {
    let mut names: Vec<(String, PathBuf)> = std::fs::read_dir(host)?
        .flatten()
        .map(|entry| (entry.file_name(), entry.path()))
        // A file that is not one of ours is not an error and not a line: a directory holding
        // other things is the normal case, which is the difference from a root that was only
        // ever meant to hold stores.
        .filter(|(_, path)| Store::looks_like_one(path))
        .filter_map(|(name, path)| name.into_string().ok().map(|name| (name, path)))
        .collect();
    names.sort();

    let mut listed = Vec::with_capacity(names.len());
    let mut refused = Vec::new();
    for (name, path) in names {
        match Store::try_from_file(&path).and_then(|store| store.count()) {
            Ok(docs) => listed.push((name, Some(docs))),
            Err(e) => {
                refused.push(format!("{name}: {e}"));
                listed.push((name, None));
            }
        }
    }
    Ok((listed, refused))
}

/// `drop` — the store file, gone. What it was built from is not.
async fn drop_store(call: &ExecCall, mount: Option<&dyn Mount>, store: &str) -> ExecResult {
    let host = match host_path(call, mount, store) {
        Ok(host) => host,
        Err(refusal) => return refusal,
    };
    let named = store.to_owned();
    let name = call.name.clone();

    let gone = tokio::task::spawn_blocking(move || {
        if !host.try_exists()? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such store",
            ));
        }
        // **The whole of what this does that `rm` does not.** A store is a path, so a mistyped
        // argument is a path to something else — and deleting it because a command called
        // `drop` was run would be this executable destroying data it was never given.
        if !Store::looks_like_one(&host) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "not a store, so this will not remove it",
            ));
        }
        std::fs::remove_file(&host)?;
        // A rollback journal beside a store that no longer exists is litter, and only ever
        // exists at all if something died mid-write. Removing the store is what makes it safe
        // to remove; a failure here is not worth failing the command over.
        //
        // Appended and not `with_extension`: SQLite names it `<file>-journal`, so a store
        // called `notes.sqlite` has `notes.sqlite-journal` beside it, where `with_extension`
        // would have replaced `sqlite` and named a different file.
        let mut journal = host.clone().into_os_string();
        journal.push("-journal");
        let _ = std::fs::remove_file(PathBuf::from(journal));
        Ok(())
    })
    .await;

    match gone {
        Ok(Ok(())) => ExecResult::ok(format!("dropped {named}\n")),
        Ok(Err(e)) => ExecResult::failed(1, format!("{name}: {named}: {e}\n")),
        Err(e) => ExecResult::failed(1, format!("{name}: {named}: {e}\n")),
    }
}

/// Parse `call.args`, or the answer clap already wrote.
///
/// [`as_registered`] is what makes a `Command` answer to the name the call was made under
/// rather than to one baked in here, and what keeps colour out of bytes on a wire.
fn parse(call: &ExecCall) -> Result<Cli, ExecResult> {
    let matches = as_registered(Cli::command(), call)
        .try_get_matches_from(&call.args)
        .map_err(rendered_by_clap)?;
    Cli::from_arg_matches(&matches).map_err(rendered_by_clap)
}
