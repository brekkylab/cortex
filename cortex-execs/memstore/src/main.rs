//! `memstore` — a store that is one file, written by whoever calls it.
//!
//! ```text
//! memstore init   notes.sqlite
//! memstore insert notes.sqlite "User switched to oat milk" "User drinks tea"
//! memstore search notes.sqlite "oat milk"
//! ```
//!
//! # What it does not do
//!
//! It does not decide. What is handed to `insert` is what the store holds, word for word — no
//! model reads it, nothing shortens or rephrases it, and there is no provider or key anywhere in
//! this program. An `insert` either writes what it was given or says why it could not; it cannot
//! cost money and it cannot fail on a network.
//!
//! That is what makes it usable by something that has already done its own deciding — a tool
//! writing down a result, an agent recording a conclusion, a person. Anything that wants a
//! judgement made about a conversation first has to make it before calling.
//!
//! # What it shares with `docstore`
//!
//! The file. Same tables, same `meta`, same write — see [`cortex_exec_storebase::sqlite`] — and
//! `meta.kind` is what tells one from the other, since the structure is what they share. What
//! differs is which columns each fills: a memory has an `id` and no `path`, so nothing collides
//! and the same sentence can be stored twice; a document has a `path`, which is its identity and
//! what makes `docstore ingest` idempotent.

mod memory;
mod store;

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Parser as _;

use crate::{memory::Memory, store::Store};

/// The name every message this program writes about itself is spelled with.
///
/// Taken from the target rather than written out, so that it is the name a caller actually typed
/// and the same one clap puts in the usage line.
const NAME: &str = env!("CARGO_BIN_NAME");

/// What `memstore` accepts and what it says about it.
#[derive(Debug, clap::Parser)]
#[command(
    about = "memstore — a store that is one file, written by whoever calls it",
    after_help = "<store> is a path like any other this program is given, so a relative one resolves against the working directory. `init` creates it; every other command expects it to be there already.",
    // The doc comments here are for whoever reads this file. What a caller of the command sees is
    // `about` and the first line of each item's; nothing below argues a design decision at
    // somebody who typed `--help`.
    long_about = None,
    // Bare `memstore` is a line that asked for nothing, and the help is the useful answer to it.
    arg_required_else_help = true,
    // `memstore help insert` as well as `memstore insert --help` is a second spelling of one thing.
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// What a command line asked for.
#[derive(Clone, Debug, PartialEq, Eq, clap::Subcommand)]
enum Command {
    /// make a store at <store>
    ///
    /// The one command that brings a store into being. Every other one is about memories and
    /// expects the file to be there, which is what keeps a mistyped name from becoming an empty
    /// store that answers every search with nothing.
    #[command(long_about = None)]
    Init(Init),

    /// remember each of <texts>
    ///
    /// The store is named per command rather than configured once for the process because a
    /// store is a document: which memories belong to what a caller is doing is their business,
    /// and two stores side by side in one directory is the normal case.
    #[command(visible_alias = "add", long_about = None)]
    Insert(Insert),

    /// the memories nearest <query>, nearest first
    #[command(long_about = None)]
    Search(Search),
}

/// `memstore init <store>` — a store where there was no file.
#[derive(Clone, Debug, PartialEq, Eq, clap::Args)]
struct Init {
    /// the store to make
    ///
    /// A path and not a name: what this ends up as is a file, and typing it as one is what keeps
    /// a caller from being handed a `String` that has to be remembered to be a path three
    /// functions later.
    #[arg(long_help = None)]
    store: PathBuf,
}

/// `memstore insert <store> <texts>...` — the memories to keep, as the caller wrote them.
#[derive(Clone, Debug, PartialEq, Eq, clap::Args)]
struct Insert {
    /// the store to write to
    #[arg(long_help = None)]
    store: PathBuf,

    /// what to remember: one memory per argument
    ///
    /// A memory per argument rather than one argument holding several. A command line is already
    /// a list of strings, so argv carries the list; a separator inside a single argument would be
    /// a second list nested in the first, and a memory containing that separator could not be
    /// written at all.
    ///
    /// Nothing here shortens, splits or rephrases what it is given. That is the whole difference
    /// between this command and one that reads a conversation for what is worth keeping — here
    /// the caller has already decided, and a second opinion applied to their decision would be a
    /// store holding something nobody wrote.
    //
    // Trimmed and refused empty at the parser, so that `Memory::text` is non-empty by
    // construction everywhere below. It belongs here and not at the write for the reason the exit
    // codes draw: a blank argument is a line that was *not understood*, which leaves with `2` and
    // names the argument, where deciding it at the store would make it a `1` after a file had
    // been opened on behalf of an argument that was never usable.
    #[arg(long_help = None, value_name = "TEXTS", value_parser = trimmed)]
    texts: Vec<String>,
}

/// `text` without its edges, or the refusal to take it as a memory.
///
/// A memory is a statement that stands on its own, and whitespace is not one. Trimming rather
/// than refusing what merely *has* edges, because a caller piping a line in has a newline on it
/// and meant the sentence.
fn trimmed(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("a memory is a statement, and this is empty".into());
    }
    Ok(text.to_owned())
}

/// `memstore search <store> <query>` — the memories in one store nearest what was asked.
#[derive(Clone, Debug, PartialEq, Eq, clap::Args)]
struct Search {
    /// the store to search
    #[arg(long_help = None)]
    store: PathBuf,

    /// what to look for
    ///
    /// A question or a phrase rather than a pattern: what comes back is what is *near* this, so
    /// there is nothing here to match and nothing to escape.
    #[arg(long_help = None)]
    query: String,

    /// how many memories to answer with
    ///
    /// A search that answered with everything it could rank would be answering with the store:
    /// the memories are ordered by how near they are, and past the first few that nearness is a
    /// formality — a memory sharing one common word with the question is on the list. So the
    /// bound is part of what makes the answer an answer, and the flag is for the caller who wants
    /// a longer one rather than a way to switch a limit on.
    #[arg(short = 'n', long, default_value_t = 10, long_help = None)]
    limit: usize,
}

/// The line a command that could not be carried out leaves on stderr.
///
/// One place that spells a store's failures, and the store named the way the caller named it: a
/// path they typed is the one thing in the message they can act on.
fn refused(store: &Path, said: impl std::fmt::Display) -> String {
    format!("{NAME}: {}: {said}\n", store.display())
}

/// The command, done: what to say on stdout, or the line to say on stderr instead.
///
/// Every way a command can end is decided here rather than on the way out, because what a reader
/// wants to know about a message is which case produced it. Whether a store could be opened is
/// the only kind of failure left by this point — a line that was not understood never reaches
/// here, clap having already answered it.
fn run(command: Command) -> Result<String, String> {
    match command {
        Command::Init(Init { store }) => match Store::try_new(&store) {
            // The store, named as it was asked for, and nothing else: a line a script can read as
            // the path it now has. What went right needs no sentence — the file is the answer.
            Ok(_) => Ok(format!("{}\n", store.display())),
            // The one failure worth a sentence of its own. `File exists` is true and says nothing
            // about what to do, where the caller is either looking at a store they already have —
            // and wanted the command that writes to one — or at a name they did not mean to type.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(refused(
                &store,
                format_args!(
                    "there is already a file here; `{NAME} insert` writes to a store that exists"
                ),
            )),
            Err(e) => Err(refused(&store, e)),
        },
        Command::Insert(Insert { store, texts }) => {
            // The store before anything else: `insert` writes to a store that exists, and a
            // caller who named the wrong file should learn it from the file.
            let opened = Store::try_from_file(&store).map_err(|e| refused(&store, e))?;

            // Straight through: what the caller wrote is what is written. There is nothing
            // between the argument and the row — no reading of it, no store consulted about what
            // it already holds — because the deciding this command does not do is the whole of
            // what it is for.
            //
            // Which also means it does not deduplicate. Told the same thing twice, the store
            // holds it twice, and that is the caller's to know rather than something to be
            // quietly saved from: two callers writing the same sentence about different things is
            // a case no comparison of text could tell from a repeat.
            let memories: Vec<Memory> = texts.into_iter().map(|text| Memory { text }).collect();

            // Written before anything is said about them, so that what a caller reads on stdout
            // is what the store holds: a write that failed after the lines were printed would
            // have told them the opposite of what happened.
            opened.insert(&memories).map_err(|e| refused(&store, e))?;

            // The memories back, one per line — the same lines `search` prints, and the same
            // shape `docstore` answers in, so that something driving either reads one kind of
            // output. What it adds over the arguments the caller already had is that these are
            // the ones the store now holds. Nothing to write is no output at all, which is the
            // answer a script producing an empty list wants.
            let mut said = String::new();
            for memory in &memories {
                said.push_str(&memory.text);
                said.push('\n');
            }
            Ok(said)
        }
        Command::Search(Search {
            store,
            query,
            limit,
        }) => {
            let opened = Store::try_from_file(&store).map_err(|e| refused(&store, e))?;
            let found = opened
                .search(&query, limit)
                .map_err(|e| refused(&store, e))?;

            // One memory per line, nearest first, and the same lines `insert` prints — what a
            // search answers with is memories, and a memory is the statement. Nothing near the
            // query is no lines and a zero: the store was read and it holds nothing near this,
            // which is an answer and not a failure. A caller that wants to act on emptiness reads
            // no lines, which is the same test they would make of any command that lists things.
            let mut said = String::new();
            for text in &found {
                said.push_str(text);
                said.push('\n');
            }
            Ok(said)
        }
    }
}

/// A line in, and what it produced out.
///
/// No `.env` and no runtime: nothing here reads a key or reaches a provider, and the one thing
/// this program waits on is a file, which it waits on in this thread.
fn main() -> ExitCode {
    // `parse` and not a result to inspect: a line that was not understood, and `--help`, are
    // both clap's to answer — help on stdout with a `0`, usage on stderr with a `2`, which is
    // what a caller reading one out of a pipe expects of any program.
    let (said, code) = match run(Cli::parse().command) {
        Ok(said) => (said, ExitCode::SUCCESS),
        Err(said) => {
            // Straight out, because this is the last thing that happens.
            std::io::stderr().write_all(said.as_bytes()).ok();
            return ExitCode::FAILURE;
        }
    };

    // Written as bytes: this is program output on its way to whatever the shell pointed at, and a
    // caller piping it into something byte-oriented has to get what was produced.
    std::io::stdout().write_all(said.as_bytes()).ok();
    code
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;
    use tempfile::TempDir;

    use super::*;

    /// Through the same parser the program runs on, so that what these pin is what runs. The
    /// program's own name in front, because that is what `argv` carries.
    fn parse(line: &[&str]) -> Result<Command, clap::Error> {
        Cli::try_parse_from(std::iter::once(NAME).chain(line.iter().copied()))
            .map(|cli| cli.command)
    }

    /// A line, parsed and carried out, as typing it would.
    fn run_line(line: &[&str]) -> Result<String, String> {
        run(parse(line).expect("the line parses"))
    }

    /// A name in the directory these tests work in, spelled absolutely: a relative one would
    /// resolve against a working directory the whole test binary shares.
    fn at(dir: &TempDir, name: &str) -> String {
        dir.path()
            .join(name)
            .to_str()
            .expect("a temporary directory is UTF-8")
            .to_owned()
    }

    /// A store is all `init` needs to be told.
    #[test]
    fn a_store_is_made_by_naming_it() {
        let Command::Init(init) =
            parse(&["init", "notes.sqlite"]).expect("a store is all init needs to be told")
        else {
            panic!("init was asked for");
        };
        assert_eq!(init.store, Path::new("notes.sqlite"));
    }

    /// End to end, as a caller would: a file where there was none, and the second attempt at the
    /// same name refused rather than quietly handed the store that is already there.
    #[test]
    fn init_makes_a_store_and_will_not_make_it_twice() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = at(&dir, "notes.sqlite");

        let made = run_line(&["init", &store]).expect("a store where there was no file");
        assert_eq!(made, format!("{store}\n"));
        assert!(dir.path().join("notes.sqlite").is_file());

        let said = run_line(&["init", &store]).expect_err("the name is taken");
        assert!(
            said.contains(&store),
            "the store is named as the caller named it: {said}"
        );
    }

    /// The whole line, and the property this command is for: what the caller wrote is what the
    /// store holds, word for word, and it is findable straight after.
    #[test]
    fn insert_writes_what_it_was_given() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = at(&dir, "notes.sqlite");
        run_line(&["init", &store]).expect("a store");

        let written = run_line(&[
            "insert",
            &store,
            "User switched to oat milk",
            "User drinks tea",
        ])
        .expect("the memories are written");
        assert_eq!(
            written, "User switched to oat milk\nUser drinks tea\n",
            "one memory per line, in the order they were given"
        );

        // And in the store, not merely echoed: asked for, it comes back.
        assert_eq!(
            run_line(&["search", &store, "oat milk"]).expect("the store answers"),
            "User switched to oat milk\n"
        );
    }

    /// A store that is not there ends the line, and nothing is created on the way past.
    #[test]
    fn insert_into_a_store_that_is_not_there_says_so() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = at(&dir, "missing.sqlite");

        let said = run_line(&["insert", &store, "User switched to oat milk"])
            .expect_err("there is no such store");
        assert!(said.contains(&store), "{said}");
        assert!(
            !dir.path().join("missing.sqlite").exists(),
            "a failed insert made nothing"
        );
    }

    /// Nothing to write is a no-op and a zero — the answer a script with an empty list wants,
    /// rather than a case it has to check for before it can call this at all.
    #[test]
    fn insert_with_nothing_to_write_writes_nothing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = at(&dir, "notes.sqlite");
        run_line(&["init", &store]).expect("a store");

        assert!(
            run_line(&["insert", &store])
                .expect("nothing is a batch too")
                .is_empty(),
            "no memories, so no lines"
        );
    }

    /// Whitespace comes off, and what is nothing but whitespace is not a memory at all.
    ///
    /// A `2` and not a `1`: the argument was never usable, which is a line to reissue rather than
    /// work that was attempted and failed — and clap names which argument, which a check made
    /// later at the store could not.
    #[test]
    fn an_empty_memory_is_a_misunderstood_line() {
        let Command::Insert(insert) =
            parse(&["insert", "notes.sqlite", "  spaced  \n"]).expect("edges come off a memory")
        else {
            panic!("insert was asked for");
        };
        assert_eq!(insert.texts, ["spaced"]);

        for blank in ["", "   ", "\n\t "] {
            let e = parse(&["insert", "notes.sqlite", blank]).expect_err("a memory is a statement");
            assert_eq!(e.kind(), ErrorKind::ValueValidation);
            assert_eq!(e.exit_code(), 2);
            assert!(
                e.to_string().contains("TEXTS"),
                "clap names the argument that was wrong: {e}"
            );
        }
    }

    /// One blank argument fails the line rather than being dropped from a list that then looks
    /// complete: a store written from what was left would be missing a memory nobody was told
    /// about.
    #[test]
    fn one_empty_memory_fails_the_whole_line() {
        let e = parse(&["insert", "notes.sqlite", "User drinks tea", "  "])
            .expect_err("the second memory is empty");
        assert_eq!(e.kind(), ErrorKind::ValueValidation);
    }

    /// `search` end to end, as a caller types it: memories in a store, a question asked of it,
    /// and the answer on stdout nearest first.
    #[test]
    fn search_answers_with_the_memories_nearest_the_question() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = at(&dir, "notes.sqlite");
        run_line(&["init", &store]).expect("a store");
        run_line(&[
            "insert",
            &store,
            "User drinks tea",
            "User switched to oat milk",
        ])
        .expect("the memories are written");

        assert_eq!(
            run_line(&["search", &store, "oat milk"]).expect("the store answers"),
            "User switched to oat milk\n"
        );

        // The bound reaches the store: two memories are near "user", and one line comes back.
        let one = run_line(&["search", &store, "user", "-n", "1"]).expect("the store answers");
        assert_eq!(one.lines().count(), 1);

        // Nothing near the question is no lines and a zero: the store was read, and it holds
        // nothing near this.
        assert!(
            run_line(&["search", &store, "almond"])
                .expect("the store answers")
                .is_empty()
        );
    }

    /// A store that is not there is the answer, and the same one `insert` gives: a search that
    /// made an empty store would answer every question with nothing, forever.
    #[test]
    fn searching_a_store_that_is_not_there_says_so() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store = at(&dir, "missing.sqlite");

        let said = run_line(&["search", &store, "oat milk"]).expect_err("there is no such store");
        assert!(said.contains(&store), "{said}");
        assert!(
            !dir.path().join("missing.sqlite").exists(),
            "searching made nothing"
        );
    }

    /// A search is a store and a question, and the count is the caller's to raise.
    #[test]
    fn a_search_is_bounded_whether_or_not_the_caller_says_so() {
        let Command::Search(search) =
            parse(&["search", "notes.sqlite", "oat milk"]).expect("a store and a question")
        else {
            panic!("search was asked for");
        };
        assert_eq!(search.query, "oat milk");
        assert_eq!(search.limit, 10);

        let Command::Search(search) =
            parse(&["search", "notes.sqlite", "oat milk", "-n", "3"]).expect("a shorter answer")
        else {
            panic!("search was asked for");
        };
        assert_eq!(search.limit, 3);
    }

    /// Help is an answer, not a refusal: a zero, so a caller can pipe it, and the name they typed
    /// in the usage line.
    #[test]
    fn help_is_an_answer() {
        let e = parse(&["--help"]).expect_err("help is not a command");
        assert_eq!(e.kind(), ErrorKind::DisplayHelp);
        assert_eq!(e.exit_code(), 0);

        // The name this was built as, not one written out here: the usage line has to tell a
        // caller a command they actually have.
        let said = e.render().to_string();
        assert!(said.contains(&format!("Usage: {NAME}")), "{said}");
        for command in ["init", "insert", "search"] {
            assert!(said.contains(command), "{command} is missing from: {said}");
        }
    }
}
