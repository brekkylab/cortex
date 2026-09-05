//! `docstore` — full-text search over a directory tree, kept in a store that is one file.
//!
//! ```text
//! docstore init   notes.db
//! docstore ingest notes.db docs          # a directory means everything under it
//! docstore search notes.db -n 5 borrow checker
//! docstore sync   notes.db docs          # the store says what the tree says
//! docstore purge  notes.db docs/old      # ingest undone
//! docstore list                          # the stores beside this one
//! docstore drop   notes.db
//! ```
//!
//! # What this is, next to `memstore`
//!
//! `memstore` takes text a caller hands it; this takes files, and remembers where each one came
//! from. That is the whole difference between them as commands — and it is why the answer to a
//! search here names a path the caller can open, rather than only the text.
//!
//! The store is SQLite's FTS5, the engine `memstore` uses, so the two link one search engine
//! rather than two. What they share and where they part is written down in
//! [`cortex_exec_storebase::sqlite`]: one set of tables, and `meta.kind` to tell a store of one
//! from a store of the other.
//!
//! # A document is known by the path it was given under
//!
//! `path` is a document's identity in the store, and it is the argument as the caller spelled it
//! with nothing but `.` taken out. A relative one stays relative, which is what makes a store
//! that lives beside what it indexes copyable along with it; an absolute one stays absolute,
//! because a caller who named a file that way meant that file wherever this is run from.
//!
//! **So a relative path means what it meant to the shell that typed it.** `docstore ingest
//! notes.db docs` from one directory and from another are two different corpora as far as the
//! store is concerned, in exactly the way `ls docs` is two different listings — and `sync` and
//! `purge`, which name documents by the same paths `ingest` filed them under, are read the same
//! way. Nothing here canonicalizes on the caller's behalf: a store that answered with a path
//! nobody typed would be answering about a file the caller cannot name.
//!
//! # Vectors: the schema is ready, and there is nothing in it
//!
//! A store carries a `chunk` table with an `embedding` column, and `meta` has room for the model
//! and the dimension that produced them. **All of it is empty.** There is no embedder in this
//! workspace to fill it — ailoy, the only language-model runtime here, has no embedding API — and
//! choosing a chunk size before there is a model to size chunks for would be deciding on no
//! evidence.
//!
//! Since a document's body is stored whole, chunks stay derivable at any later time: the day an
//! embedder arrives, the work is a backfill rather than a re-ingest. Recording the model in
//! `meta` is what makes that backfill able to notice that the vectors it finds were written by a
//! different model, which is the difference between re-embedding and ranking wrongly in silence.

mod ingest;
mod search;
mod store;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser as _, Subcommand};

use crate::search::DEFAULT_LIMIT;
use crate::store::Store;

/// The name every message this program writes about itself is spelled with.
///
/// Taken from the target rather than written out, so that it is the name a caller actually typed
/// and the same one clap puts in the usage line.
const NAME: &str = env!("CARGO_BIN_NAME");

// The parse of one line.
//
// A real parser and not a `match` on the first argument, because `--help` has to work on every
// subcommand and not only on the name — which is the whole of what an agent has to go on once it
// has decided to run this.
//
// `//` and not `///` throughout this file's clap types: a doc comment there is what `--help`
// prints, so anything a *caller* would not act on belongs in a comment instead.
#[derive(clap::Parser)]
#[command(
    about = "index files from a directory tree and search them",
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = "<STORE> is a path like any other this program is given. `init` makes one; every other command expects it to be there already."
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
        /// Where to make it — `notes.db`.
        #[arg(value_name = "STORE")]
        store: PathBuf,
    },

    /// Index files into a store.
    ///
    /// A directory means everything under it. Re-ingesting a path replaces it, so running this
    /// twice means the same as running it once.
    Ingest {
        /// Which store to write.
        #[arg(value_name = "STORE")]
        store: PathBuf,

        /// What to index — `docs`, `docs/a.md`. A document is filed under the path as it is
        /// written here, so this is also what `sync` and `purge` name it by.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },

    /// Search a store.
    Search {
        /// Which store to ask.
        #[arg(value_name = "STORE")]
        store: PathBuf,

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
        store: PathBuf,

        /// Paths, spelled as `ingest` spelled them. A path that is no longer there means
        /// everything the store held under it goes.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,

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
        store: PathBuf,

        /// Paths, spelled as `ingest` spelled them.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },

    /// The stores in a directory, and how much is in each.
    ///
    /// Looks at that one directory and does not descend into it. Files that are not stores are
    /// passed over in silence, since a directory holding other things is the normal case.
    List {
        /// Which directory to look in. The one this was run in, if it is left out.
        #[arg(value_name = "DIR", default_value = ".")]
        dir: PathBuf,
    },

    /// Remove a store. The files it was built from are untouched.
    ///
    /// Refuses anything that is not a store, which is the whole of what this does that `rm`
    /// does not: a store is a path, so one mistyped argument could otherwise be somebody's
    /// data.
    Drop {
        /// Which store to forget.
        #[arg(value_name = "STORE")]
        store: PathBuf,
    },
}

/// What a command produced: what to say, and whether it worked.
///
/// Both streams and not one or the other, because [`list`] fills both: a store that will not open
/// is named on stderr beside the listing of the ones that did. Every other command writes one of
/// them and leaves the other empty.
struct Answer {
    out: String,
    err: String,
    ok: bool,
}

impl Answer {
    fn ok(out: impl Into<String>) -> Answer {
        Answer {
            out: out.into(),
            err: String::new(),
            ok: true,
        }
    }

    /// A command that was understood and did not work. Always `1`, which is the difference a
    /// caller acts on: clap answers a line it could not read with `2`, and that line can be
    /// reissued differently where this one was already the right one.
    fn failed(err: impl Into<String>) -> Answer {
        Answer {
            out: String::new(),
            err: err.into(),
            ok: false,
        }
    }
}

/// The command, done.
fn run(command: Command) -> Answer {
    match command {
        Command::Init { store } => init(&store),

        Command::Ingest { store, paths } => match opened(&store) {
            Ok(store) => reported("ingest", ingest::run(&store, &paths)),
            Err(refusal) => refusal,
        },

        Command::Search {
            store,
            limit,
            query,
        } => match opened(&store) {
            Ok(store) => reported("search", search::run(&store, limit, &query.join(" "))),
            Err(refusal) => refusal,
        },

        Command::Sync {
            store,
            paths,
            force,
        } => match opened(&store) {
            Ok(store) => reported("sync", ingest::sync(&store, &paths, force)),
            Err(refusal) => refusal,
        },

        Command::Purge { store, paths } => match opened(&store) {
            Ok(store) => reported("purge", ingest::purge(&store, &paths)),
            Err(refusal) => refusal,
        },

        Command::List { dir } => list(&dir),

        Command::Drop { store } => drop_store(&store),
    }
}

/// What one of the commands that works through the store has to say for itself.
fn reported(what: &str, done: std::io::Result<String>) -> Answer {
    match done {
        Ok(report) => Answer::ok(report),
        Err(e) => Answer::failed(format!("{NAME}: {what}: {e}\n")),
    }
}

/// `init` — the file, and its schema.
fn init(store: &Path) -> Answer {
    match Store::try_new(store) {
        // The store, named as it was asked for, and nothing else: a line a script can read as
        // the path it now has. What went right needs no sentence — the file is the answer.
        Ok(_) => Answer::ok(format!("{}\n", store.display())),
        // The one failure worth a sentence of its own. `File exists` is true and says nothing
        // about what to do, where the caller is either looking at a store they already have —
        // and wanted the command that writes to one — or at a path they did not mean to type.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Answer::failed(format!(
            "{NAME}: {}: there is already a file here; \
             `{NAME} ingest` writes to a store that exists\n",
            store.display()
        )),
        Err(e) => Answer::failed(refusal(store, e)),
    }
}

/// The store a path names, opened — or the refusal to answer with.
fn opened(store: &Path) -> Result<Store, Answer> {
    Store::try_from_file(store).map_err(|e| Answer::failed(refusal(store, e)))
}

/// One place that spells what went wrong with a path the caller named.
fn refusal(path: &Path, e: std::io::Error) -> String {
    format!("{NAME}: {}: {e}\n", path.display())
}

/// `list` — the stores in one directory, with the number of documents in each.
fn list(dir: &Path) -> Answer {
    let (listed, refused) = match stores_in(dir) {
        Ok(found) => found,
        Err(e) => return Answer::failed(refusal(dir, e)),
    };

    if listed.is_empty() && refused.is_empty() {
        return Answer::ok(format!(
            "no stores here — `{NAME} init <STORE>` makes one\n"
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
        return Answer::ok(out);
    }
    // A store that will not open is still a store, and naming it beside the others is more use
    // than failing the whole listing over one of them. The exit code still says something went
    // wrong, because a caller asking what it has should not read a broken store as an answer.
    Answer {
        out,
        err: format!("{}\n", refused.join("\n")),
        ok: false,
    }
}

/// Every store directly in `dir`, with its document count, and the ones that would not open.
///
/// Sorted by name, and not recursive: `list` answers about a directory, and a walk would turn a
/// question about what is here into a question about everything below.
#[allow(clippy::type_complexity)]
fn stores_in(dir: &Path) -> std::io::Result<(Vec<(String, Option<u64>)>, Vec<String>)> {
    let mut names: Vec<(String, PathBuf)> = std::fs::read_dir(dir)?
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
fn drop_store(store: &Path) -> Answer {
    let gone = (|| {
        if !store.try_exists()? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such store",
            ));
        }
        // **The whole of what this does that `rm` does not.** A store is a path, so a mistyped
        // argument is a path to something else — and deleting it because a command called `drop`
        // was run would be this program destroying data it was never given.
        if !Store::looks_like_one(store) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "not a store, so this will not remove it",
            ));
        }
        std::fs::remove_file(store)?;
        // A rollback journal beside a store that no longer exists is litter, and only ever exists
        // at all if something died mid-write. Removing the store is what makes it safe to remove;
        // a failure here is not worth failing the command over.
        //
        // Appended and not `with_extension`: SQLite names it `<file>-journal`, so a store called
        // `notes.sqlite` has `notes.sqlite-journal` beside it, where `with_extension` would have
        // replaced `sqlite` and named a different file.
        let mut journal = store.to_path_buf().into_os_string();
        journal.push("-journal");
        let _ = std::fs::remove_file(PathBuf::from(journal));
        Ok(())
    })();

    match gone {
        Ok(()) => Answer::ok(format!("dropped {}\n", store.display())),
        Err(e) => Answer::failed(refusal(store, e)),
    }
}

/// A line in, and what it produced out.
///
/// No runtime and no thread pool: a walk of a tree and a SQLite write are the slow kind of work,
/// and this program has nothing else to get on with while they happen.
fn main() -> ExitCode {
    // `parse` and not a result to inspect: a line that was not understood, and `--help`, are both
    // clap's to answer — help on stdout with a `0`, usage on stderr with a `2`, which is what a
    // caller reading one out of a pipe expects of any program.
    let answer = run(Cli::parse().command);

    // Written as bytes: this is program output on its way to whatever the shell pointed at, and a
    // caller piping it into something byte-oriented has to get what was produced.
    std::io::stdout().write_all(answer.out.as_bytes()).ok();
    std::io::stderr().write_all(answer.err.as_bytes()).ok();
    if answer.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
