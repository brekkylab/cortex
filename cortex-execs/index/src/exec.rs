//! `<name> <ingest|search|list|drop> ...` — the parser, and the branch on what it produced.

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::{ColorChoice, CommandFactory, FromArgMatches, Parser, Subcommand};
use cortex::exec::{ExecCall, ExecResult, Executable};
use cortex::fs::Mount;
use futures_core::future::BoxFuture;

use crate::search::DEFAULT_LIMIT;
use crate::store::Store;
use crate::{ingest, search};

/// Full-text search over the tree a session is working in.
///
/// # A store is named, and the name is not a path
///
/// Every subcommand names the index it works on, so a session can keep more than one and say
/// which it means — the same shape `mem` has, for the same reason: an index is a thing a
/// session has several of, not a setting it was configured with.
///
/// What differs from `mem` is *where* that thing lives, and it differs because the two are
/// different kinds of thing. A `mem` store is a **document**: one file of the tree, which the
/// agent may read, copy or commit. An index is **derived state** — nobody reads it directly
/// and deleting it costs a re-ingest — so it has no business in the tree, and there is a
/// harder reason besides: tantivy `mmap`s its segments and takes a lock file, and under
/// FUSE-T a mount is an NFS one, where those are exactly the operations that behave
/// differently.
///
/// So a store is a **name under a root this executable was given**, and the root is on the
/// host. That the caller cannot name the root is the point: `<STORE>` must be one path
/// component, so every index this can reach is one somebody meant it to have.
pub struct Index {
    root: PathBuf,
    /// Stores opened so far, by name.
    ///
    /// Cached because tantivy allows exactly one `IndexWriter` per index: opening per call
    /// would make two overlapping ingests of one store collide on its lock file.
    ///
    /// Nothing evicts, so a session that touches many stores keeps a reader (and its mapped
    /// segments) for each. That is the same question [`Store::writer`] answers about the
    /// writer, and has the same answer for now: no consumer has said it matters.
    open: Mutex<BTreeMap<String, Arc<Store>>>,
}

impl Index {
    /// What [`register`](cortex::exec::ExecutableSet::register) wants: one line for a list,
    /// not usage.
    pub const SUMMARY: &'static str = "index files from the workspace and search them";

    /// Keep every index under `root`, creating it if it is not there.
    ///
    /// **`root` should not be inside a FUSE mount**, for the reason in the type's docs — and
    /// that is a rule for the caller rather than a check here, because nothing in this
    /// process can tell one [`Mount`] from a plain directory. The standalone binary is the
    /// case that proves the check could not be written: its "mount" is the working directory
    /// and its root sits inside it, which is correct there and would be wrong under FUSE.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Index {
            root,
            open: Mutex::new(BTreeMap::new()),
        })
    }

    /// Where the store called `name` lives.
    ///
    /// **`name` is one path component and nothing else.** A separator, a `..` or an absolute
    /// path would reach outside the root, and the root is the whole of what bounds this — a
    /// delegated name gets to say *which* index, never *where*.
    fn dir_of(&self, name: &str) -> io::Result<PathBuf> {
        // A separator anywhere, before the components are even looked at: `notes/` parses as
        // one component, so a walk of them alone would accept a spelling that is a path.
        let mut components = Path::new(name).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(one)), None)
                if !name.starts_with('.') && !name.contains('/') =>
            {
                Ok(self.root.join(one))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidFilename,
                format!("{name}: a store is one name, not a path"),
            )),
        }
    }

    /// The store called `name`, opened or created.
    fn store(&self, name: &str) -> io::Result<Arc<Store>> {
        let dir = self.dir_of(name)?;
        if let Some(store) = self.cached(name) {
            return Ok(store);
        }

        // Opened without the map held: `Store::open` reads a directory and may build an
        // index, and holding the table through that would make opening one store block a
        // lookup of every other. The cost is that two calls can open the same store at once,
        // which the check below settles by keeping whichever landed first, so that one name
        // still means one writer.
        let opened = Store::open(&dir)?;
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        Ok(open.entry(name.to_owned()).or_insert(opened).clone())
    }

    fn cached(&self, name: &str) -> Option<Arc<Store>> {
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// The store called `name`, which must already be there.
    ///
    /// What `search` and `drop` take, so that a typo is an error rather than an empty index
    /// answering "no matches" — which reads as a corpus with nothing in it.
    fn existing(&self, name: &str) -> io::Result<Arc<Store>> {
        if !self.dir_of(name)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{name}: no such store — `list` says what there is"),
            ));
        }
        self.store(name)
    }

    /// The stores that exist, sorted. A directory read, and cheap enough to do per call.
    fn names(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| !name.starts_with('.'))
            .collect();
        names.sort();
        names
    }

    /// Forget an index. The corpus it was built from is untouched.
    fn drop_store(&self, name: &str) -> io::Result<String> {
        let dir = self.dir_of(name)?;
        if !dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{name}: no such store"),
            ));
        }
        // Out of the cache first: whoever still holds an `Arc` goes on working against files
        // that are being removed, and that is one call's problem rather than every later one's.
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        std::fs::remove_dir_all(&dir)?;
        Ok(format!("dropped {name}\n"))
    }
}

// The parse of one call.
//
// A real parser and not a `match` on the first argument, because `--help` has to work on
// every subcommand and not only on the name — which is the whole of what an agent has to go
// on once it has decided to call this. Nothing here intercepts an argument: a delegated name
// is a program on the guest's `PATH`, and a program answers its own help.
//
// `//` and not `///` throughout this file's clap types: a doc comment here is what `--help`
// prints, so anything a *caller* would not act on belongs in a comment instead. clap's derive
// also takes a doc comment's first line as `about` and the rest as `long_about`, so a note
// left as `///` would quietly outrank the `about` set below.
#[derive(Parser)]
#[command(
    about = Index::SUMMARY,
    subcommand_required = true,
    arg_required_else_help = true,
    // No `--version`: a delegated name is not a package, and the version a caller could act
    // on is the console's, not this executable's.
    disable_version_flag = true,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Index files from the workspace into a store, creating the store if it is not there.
    ///
    /// A directory means everything under it. Re-ingesting a path replaces it, so running
    /// this twice means the same as running it once.
    Ingest {
        /// Which index to write. One name, not a path — `list` says what there is.
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
    ///
    /// Needs nothing mounted: the bytes were read when they were ingested.
    Search {
        /// Which index to ask.
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
    /// What a second `ingest` cannot do: it picks up what was added and replaces what
    /// changed, but leaves a document behind for a file that is gone. This removes those
    /// too, so the index says what the tree says.
    ///
    /// Only files whose size or modification time moved are read again.
    Sync {
        /// Which index to bring up to date.
        #[arg(value_name = "STORE")]
        store: String,

        /// Workspace paths, spelled as `ingest` spelled them. A path that is no longer there
        /// means everything the index held under it goes.
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
    /// A path removes the document at it and everything under it. Needs nothing mounted: a
    /// file gone from the tree is the reason to do this, and it is still in the index.
    Purge {
        /// Which index to take them out of.
        #[arg(value_name = "STORE")]
        store: String,

        /// Workspace paths, spelled as `ingest` spelled them.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
    },

    /// The stores that exist, and how much is in each.
    List,

    /// Remove a store. The files it was built from are untouched.
    Drop {
        /// Which index to forget.
        #[arg(value_name = "STORE")]
        store: String,
    },
}

impl Executable for Index {
    /// The mount reaches only `ingest`: a query is answered from the index, whose bytes were
    /// read when they were ingested. So `search` works in a console with nothing mounted,
    /// and only `ingest` has to say when it cannot reach a file.
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecResult> {
        Box::pin(async move {
            let parsed = match parse(self, call) {
                Ok(parsed) => parsed,
                Err(answer) => return answer,
            };

            match parsed.command {
                Command::Ingest { store, paths } => match self.store(&store) {
                    Ok(store) => ingest::run(&store, call, mount, &paths).await,
                    Err(e) => ExecResult::failed(1, format!("{e}\n")),
                },
                Command::Search {
                    store,
                    limit,
                    query,
                } => match self.existing(&store) {
                    Ok(store) => search::run(&store, limit, &query.join(" ")).await,
                    Err(e) => ExecResult::failed(1, format!("{e}\n")),
                },
                Command::Sync {
                    store,
                    paths,
                    force,
                } => match self.existing(&store) {
                    Ok(store) => ingest::sync(&store, call, mount, &paths, force).await,
                    Err(e) => ExecResult::failed(1, format!("{e}\n")),
                },
                Command::Purge { store, paths } => match self.existing(&store) {
                    Ok(store) => ingest::purge(&store, call, &paths).await,
                    Err(e) => ExecResult::failed(1, format!("{e}\n")),
                },
                Command::List => list(self).await,
                Command::Drop { store } => match self.drop_store(&store) {
                    Ok(said) => ExecResult::ok(said),
                    Err(e) => ExecResult::failed(1, format!("{e}\n")),
                },
            }
        })
    }
}

/// Every store with the number of documents in it.
async fn list(index: &Index) -> ExecResult {
    let names = index.names();
    if names.is_empty() {
        return ExecResult::ok("no stores yet — `ingest <STORE> <PATH>...` makes one\n");
    }
    let mut out = String::new();
    let mut refused = Vec::new();
    for name in names {
        match index.existing(&name).and_then(|store| store.num_docs()) {
            Ok(docs) => out.push_str(&format!("{name}\t{docs} document(s)\n")),
            // A store that will not open is still a store, and naming it beside the others is
            // more use than failing the whole listing over one of them. The exit code still
            // says something went wrong, because a caller asking what it has should not read
            // a broken store as an answer.
            Err(e) => {
                out.push_str(&format!("{name}\t(unreadable)\n"));
                refused.push(format!("{name}: {e}"));
            }
        }
    }
    if refused.is_empty() {
        return ExecResult::ok(out);
    }
    ExecResult {
        stdout: out.into_bytes(),
        stderr: format!("{}\n", refused.join("\n")).into_bytes(),
        exit_code: 1,
        timed_out: false,
    }
}

/// Parse `call.args`, or the answer clap already wrote.
///
/// Four things this has to get right, none of them clap's default:
///
/// * **`no_binary_name`** — [`ExecCall::args`] is everything *after* the name, so the first
///   element is a subcommand and not a program.
/// * **the name comes at run time** — one executable can be registered under several, so the
///   usage line has to say the one it was invoked by rather than a name baked in here.
///   `bin_name` as well as `name`, because the second is what a *subcommand's* usage is
///   spelled with.
/// * **never colour** — there is no tty, and this output is bytes on a wire.
/// * **the stores that exist go under each subcommand's help** — a name is not a path, so
///   nothing in the tree can be listed to find one, and `--help` is where an agent looks.
///   Below the usage rather than as the argument's possible values: `mut_arg` hands back an
///   `Arg` that has lost its positional index, which puts `store` behind the greedy `query`
///   and makes the last word of a search the store name.
///
/// The error is not a failure by itself: `--help` arrives as one, and
/// [`exit_code`](clap::Error::exit_code) is what already separates it (`0`) from a caller who
/// got the usage wrong (`2`). Nothing exits the process — a delegated call answers.
fn parse(index: &Index, call: &ExecCall) -> Result<Cli, ExecResult> {
    let mut command = Cli::command()
        .name(call.name.clone())
        .bin_name(call.name.clone())
        .no_binary_name(true)
        .color(ColorChoice::Never);

    // Nothing to say when there is nothing yet, and `ingest` is how a first one is made.
    let names = index.names();
    if !names.is_empty() {
        let listed = format!("stores: {}", names.join(", "));
        command = command.after_help(listed.clone());
        for sub in ["ingest", "search", "sync", "purge", "drop"] {
            let listed = listed.clone();
            command = command.mut_subcommand(sub, move |sc| sc.after_help(listed));
        }
    }

    let matches = command
        .try_get_matches_from(&call.args)
        .map_err(rendered_by_clap)?;
    Cli::from_arg_matches(&matches).map_err(rendered_by_clap)
}

/// clap wrote the whole message, including the trailing newline; it goes back as it is.
///
/// Help on stdout and a usage error on stderr, which is where each belongs for a caller
/// reading one out of a pipe.
fn rendered_by_clap(err: clap::Error) -> ExecResult {
    let text = err.render().to_string();
    match err.exit_code() {
        0 => ExecResult::ok(text),
        code => ExecResult::failed(code, text),
    }
}

/// The host path of a workspace path an argument named, or the refusal to answer with.
///
/// A name that cannot reach a file says so, rather than resolving against something else.
pub(crate) fn host_path(
    call: &ExecCall,
    mount: Option<&dyn Mount>,
    arg: &str,
) -> Result<PathBuf, ExecResult> {
    let Some(mount) = mount else {
        return Err(ExecResult::failed(
            1,
            format!(
                "{}: nothing is mounted, so there is no {arg} to reach\n",
                call.name
            ),
        ));
    };
    match call.resolve(arg) {
        Ok(path) => Ok(mount.host_path(&path)),
        Err(e) => Err(ExecResult::failed(1, format!("{arg}: {e}\n"))),
    }
}
