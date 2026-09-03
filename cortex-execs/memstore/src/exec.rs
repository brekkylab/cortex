//! `memstore` as a command: arguments in, lines out.

use clap::CommandFactory as _;
use cortex::{
    BoxFuture,
    exec::{ExecCall, ExecResult, Executable},
    fs::Mount,
};
use cortex_exec_storebase::command::{as_registered, host_path, rendered_by_clap};

use crate::memory::Memory;

/// The command line, as declarations — what `memstore` accepts and what it says about it.
///
/// Only shapes and their help live here; the reading of a line happens where the line is acted
/// on. `memstore` is invoked in-process, so clap's usual job — read `argv`, print, exit — is
/// exactly what must not happen: the words come from an [`ExecCall`] and the answer goes back as
/// bytes and a code, on a session that has other work to do. What is being borrowed from clap is
/// the parsing and the help, which is to say the conventions a person expects of a command line —
/// `--flag=value`, `--`, a suggestion when they mistype an option — and not the driving.
mod args {
    use std::path::{Path, PathBuf};

    use clap::Parser;

    #[derive(Debug, Parser)]
    #[command(
        // No `name`: `cortex_exec_storebase::command::as_registered` sets it per call, from the name
        // the executable was registered under. A name here would be a usage line telling a
        // caller to run a command they may not have.
        about = "memstore — a store that is one file in this tree, written by whoever calls it",
        after_help = "<store> is a path in this tree, resolved like any other argument. `init` creates it; every other command expects it to be there already.",
        // The doc comments in this module are for whoever reads the module. What a caller of the
        // command sees is `about` and the first line of each item's; nothing below argues clap's
        // design decisions at somebody who typed `--help`.
        long_about = None,
        // Bare `memstore` is a line that asked for nothing, and the help is the useful answer to it.
        arg_required_else_help = true,
        // `memstore help insert` as well as `memstore insert --help` is a second spelling of one thing.
        disable_help_subcommand = true,
        // Nothing here is versioned on its own: this ships inside whatever registered it, not as
        // a program somebody installed and might hold an old copy of.
        disable_version_flag = true
    )]
    pub struct Cli {
        #[command(subcommand)]
        pub command: Command,
    }

    /// What a command line asked for.
    #[derive(Clone, Debug, PartialEq, Eq, clap::Subcommand)]
    pub enum Command {
        /// make a store at <store>
        ///
        /// The one command that brings a store into being. Every other one is about memories
        /// and expects the file to be there, which is what keeps a mistyped name from becoming
        /// an empty store that answers every search with nothing.
        #[command(long_about = None)]
        Init(Init),

        /// remember each of <texts>
        ///
        /// The store is named per command rather than configured once for the process because a
        /// store is a document: which memories belong to what a session is doing is the caller's
        /// business, and two stores side by side in one directory is the normal case.
        #[command(visible_alias = "add", long_about = None)]
        Insert(Insert),

        /// the memories nearest <query>, nearest first
        #[command(long_about = None)]
        Search(Search),
    }

    impl Command {
        /// The store this line names, whichever command named it.
        ///
        /// Every command takes one, so asking for it is a question about the line rather than
        /// about which command was written — and the arm-per-variant that answers it belongs
        /// next to the variants, where a command added below is a compile error here and not a
        /// store quietly resolved somewhere else. What each command then *does* to the store is
        /// the difference between them, and stays where they part ways.
        pub fn get_store_path(&self) -> &Path {
            match self {
                Self::Init(init) => &init.store,
                Self::Insert(insert) => &insert.store,
                Self::Search(search) => &search.store,
            }
        }
    }

    /// `memstore init <store>` — a store where there was no file.
    #[derive(Clone, Debug, PartialEq, Eq, clap::Args)]
    pub struct Init {
        /// the store to make, as a path in this tree
        ///
        /// A path and not a name: what this ends up as is a file, and typing it as one is what
        /// keeps a caller from being handed a `String` that has to be remembered to be a path
        /// three functions later.
        #[arg(long_help = None)]
        pub store: PathBuf,
    }

    /// `memstore insert <store> <texts>...` — the memories to keep, as the caller wrote them.
    #[derive(Clone, Debug, PartialEq, Eq, clap::Args)]
    pub struct Insert {
        /// the store to write to, as a path in this tree
        #[arg(long_help = None)]
        pub store: PathBuf,

        /// what to remember: one memory per argument
        ///
        /// A memory per argument rather than one argument holding several. A command line is
        /// already a list of strings and `ExecCall::args` hands it over as one, so argv carries
        /// the list; a separator inside a single argument would be a second list nested in the
        /// first, and a memory containing that separator could not be written at all.
        ///
        /// Nothing here shortens, splits or rephrases what it is given. That is the whole
        /// difference between this command and `mem insert`, which is handed a conversation and
        /// asks a model what in it is worth keeping — here the caller has already decided, and
        /// a second opinion applied to their decision would be a store holding something
        /// nobody wrote.
        //
        // Trimmed and refused empty at the parser, so that `Memory::text` is non-empty by
        // construction everywhere below. It belongs here and not at the write for the reason
        // the exit codes draw: a blank argument is a line that was *not understood*, which
        // leaves with `2` and names the argument, where deciding it at the store would make it
        // a `1` after a mount had been resolved on behalf of an argument that was never usable.
        #[arg(long_help = None, value_name = "TEXTS", value_parser = trimmed)]
        pub texts: Vec<String>,
    }

    /// `text` without its edges, or the refusal to take it as a memory.
    ///
    /// A memory is a statement that stands on its own, and whitespace is not one. Trimming
    /// rather than refusing what merely *has* edges, because a caller piping a line in has a
    /// newline on it and meant the sentence.
    fn trimmed(text: &str) -> Result<String, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a memory is a statement, and this is empty".into());
        }
        Ok(text.to_owned())
    }

    /// `memstore search <store> <query>` — the memories in one store nearest what was asked.
    #[derive(Clone, Debug, PartialEq, Eq, clap::Args)]
    pub struct Search {
        /// the store to search, as a path in this tree
        #[arg(long_help = None)]
        pub store: PathBuf,

        /// what to look for
        ///
        /// A question or a phrase rather than a pattern: what comes back is what is *near* this,
        /// so there is nothing here to match and nothing to escape.
        #[arg(long_help = None)]
        pub query: String,

        /// how many memories to answer with
        ///
        /// A search that answered with everything it could rank would be answering with the
        /// store: the memories are ordered by how near they are, and past the first few that
        /// nearness is a formality — a memory sharing one common word with the question is on
        /// the list. So the bound is part of what makes the answer an answer, and the flag is
        /// for the caller who wants a longer one rather than a way to switch a limit on.
        #[arg(short = 'n', long, default_value_t = 10, long_help = None)]
        pub limit: usize,
    }
}

/// The `memstore` command, as a name a consumer can register.
#[derive(Debug, Clone, Default)]
pub struct MemStore {}

impl MemStore {
    /// What [`register`](cortex::exec::ExecutableSet::register) wants: one line for a list,
    /// not usage.
    pub const SUMMARY: &'static str = "keep text in a store in this tree, and search it";

    pub fn new() -> Self {
        Self {}
    }
}

/// `memstore` as a call, answering on stdout and stderr like the program it stands in for.
///
/// The whole command is here, in the one method the trait has: a line is read, the store it names
/// is found, and the work is done, in that order and in that order only. Every way it can end is
/// an [`ExecResult`] written where it is decided, because what a reader wants to know about an
/// exit code is which line produced it.
impl Executable for MemStore {
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecResult> {
        Box::pin(async move {
            let command = match as_registered(args::Cli::command(), call)
                .try_get_matches_from(&call.args)
                .and_then(|matches| {
                    <args::Cli as clap::FromArgMatches>::from_arg_matches(&matches)
                }) {
                Ok(cli) => cli.command,
                // Not a failure by itself: `--help` arrives as one, and `rendered_by_clap` is
                // what separates an answer (stdout, `0`) from a line that was not understood
                // (stderr, `2`).
                Err(e) => return rendered_by_clap(e),
            };

            // Every command names a store, so finding it happens once, before they part ways:
            // what differs between them is what they then do to it. Owned, because the command
            // is taken apart below and this name outlives it.
            let store_path = command.get_store_path().to_path_buf();

            // The `expect` is the argument's own history, not an assumption about paths: a
            // call carries its words as `ExecCall::args`, which is `Vec<String>`, so
            // every byte in this path was UTF-8 before clap ever spelled it as one.
            let named = store_path
                .to_str()
                .expect("a store named on a command line came from a `String`");
            let store = match host_path(call, mount, named) {
                Ok(host) => host,
                Err(refusal) => return refusal,
            };

            // One place that spells a store's failures. `call.name` and not a literal, because
            // this executable answers to whatever name it was registered under.
            let failed = |e: std::io::Error| {
                ExecResult::failed(1, format!("{}: {named}: {e}\n", call.name))
            };

            match command {
                args::Command::Init(_) => {
                    // The host path, because that is where a file is actually made — and the
                    // caller's name for it in everything said about it, since the host path is
                    // on the far side of a mount they cannot see.
                    match crate::store::Store::try_new(&store) {
                        // The store, named as it was asked for, and nothing else: a line a
                        // script can read as the path it now has. What went right needs no
                        // sentence — the file is the answer.
                        Ok(_) => ExecResult::ok(format!("{}\n", store_path.display())),
                        // The one failure worth a sentence of its own. `File exists` is true and
                        // says nothing about what to do, where the caller is either looking at a
                        // store they already have — and wanted the command that writes to one —
                        // or at a name they did not mean to type.
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            ExecResult::failed(
                                1,
                                format!(
                                    "{name}: {named}: there is already a file here; \
                                     `{name} insert` writes to a store that exists\n",
                                    name = call.name
                                ),
                            )
                        }
                        Err(e) => failed(e),
                    }
                }
                args::Command::Insert(insert) => {
                    // The store before anything else: `insert` writes to a store that exists, and
                    // a caller who named the wrong file should learn it from the file.
                    let store = match crate::store::Store::try_from_file(&store) {
                        Ok(store) => store,
                        Err(e) => return failed(e),
                    };

                    // Straight through: what the caller wrote is what is written. There is
                    // nothing between the argument and the row — no reading of it, no store
                    // consulted about what it already holds — because the deciding this command
                    // does not do is the whole of what it is for.
                    //
                    // Which also means it does not deduplicate. Told the same thing twice, the
                    // store holds it twice, and that is the caller's to know rather than
                    // something to be quietly saved from: two callers writing the same sentence
                    // about different things is a case no comparison of text could tell from a
                    // repeat.
                    let memories: Vec<Memory> = insert
                        .texts
                        .into_iter()
                        .map(|text| Memory { text })
                        .collect();

                    // Written before anything is said about them, so that what a caller reads on
                    // stdout is what the store holds: a write that failed after the lines were
                    // printed would have told them the opposite of what happened.
                    if let Err(e) = store.insert(&memories) {
                        return failed(e);
                    }

                    // The memories back, one per line — the same lines `search` prints, and the
                    // same shape `mem insert` answers in, so that something driving either reads
                    // one kind of output. What it adds over the arguments the caller already had
                    // is that these are the ones the store now holds. Nothing to write is no
                    // output at all, which is the answer a script producing an empty list wants.
                    let mut said = String::new();
                    for memory in &memories {
                        said.push_str(&memory.text);
                        said.push('\n');
                    }
                    ExecResult::ok(said)
                }
                args::Command::Search(search) => {
                    let store = match crate::store::Store::try_from_file(&store) {
                        Ok(store) => store,
                        Err(e) => return failed(e),
                    };

                    let found = match store.search(&search.query, search.limit) {
                        Ok(found) => found,
                        Err(e) => return failed(e),
                    };

                    // One memory per line, nearest first, and the same lines `insert` prints —
                    // what a search answers with is memories, and a memory is the statement.
                    // Nothing near the query is no lines and a zero: the store was read and it
                    // holds nothing near this, which is an answer and not a failure. A caller
                    // that wants to act on emptiness reads no lines, which is the same test they
                    // would make of any command that lists things.
                    let mut said = String::new();
                    for text in &found {
                        said.push_str(text);
                        said.push('\n');
                    }
                    ExecResult::ok(said)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use clap::error::ErrorKind;

    use super::*;

    /// The tree as the `memstore` binary supplies it: a directory on this host, with nothing
    /// mounted over it.
    struct Here(PathBuf);

    impl Mount for Here {
        fn mountpoint(&self) -> &Path {
            &self.0
        }
    }

    /// Through the same shell the command parses with, so that what these pin is what runs.
    fn parse(line: &[&str]) -> Result<args::Command, clap::Error> {
        let call = call(line);
        as_registered(args::Cli::command(), &call)
            .try_get_matches_from(&call.args)
            .and_then(|m| <args::Cli as clap::FromArgMatches>::from_arg_matches(&m))
            .map(|cli| cli.command)
    }

    fn call(args: &[&str]) -> ExecCall {
        ExecCall {
            name: "memstore".into(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            cwd: Some(String::new()),
            env: Default::default(),
        }
    }

    /// A store is all `init` needs to be told.
    #[test]
    fn a_store_is_made_by_naming_it() {
        let args::Command::Init(init) =
            parse(&["init", "notes.sqlite"]).expect("a store is all init needs to be told")
        else {
            panic!("init was asked for");
        };
        assert_eq!(init.store, Path::new("notes.sqlite"));
    }

    /// End to end, as a caller would: a file where there was none, and the second attempt at the
    /// same name refused rather than quietly handed the store that is already there.
    #[tokio::test]
    async fn init_makes_a_store_and_will_not_make_it_twice() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let here = Here(dir.path().to_path_buf());

        let made = MemStore::new()
            .exec(&call(&["init", "notes.sqlite"]), Some(&here))
            .await;
        assert_eq!(
            made.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&made.stdout), "notes.sqlite\n");
        assert!(dir.path().join("notes.sqlite").is_file());

        let again = MemStore::new()
            .exec(&call(&["init", "notes.sqlite"]), Some(&here))
            .await;
        assert_eq!(again.exit_code, 1);
        assert!(again.stdout.is_empty(), "nothing to read as a store's path");
        let said = String::from_utf8_lossy(&again.stderr);
        assert!(
            said.contains("notes.sqlite"),
            "the store is named as the caller named it: {said}"
        );
    }

    /// The whole line, and the property that separates this command from `mem insert`: what the
    /// caller wrote is what the store holds, word for word, and it is findable straight after.
    #[tokio::test]
    async fn insert_writes_what_it_was_given() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let here = Here(dir.path().to_path_buf());

        let made = MemStore::new()
            .exec(&call(&["init", "notes.sqlite"]), Some(&here))
            .await;
        assert_eq!(made.exit_code, 0);

        let written = MemStore::new()
            .exec(
                &call(&[
                    "insert",
                    "notes.sqlite",
                    "User switched to oat milk",
                    "User drinks tea",
                ]),
                Some(&here),
            )
            .await;
        assert_eq!(
            written.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&written.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&written.stdout),
            "User switched to oat milk\nUser drinks tea\n",
            "one memory per line, in the order they were given"
        );

        // And in the store, not merely echoed: asked for, it comes back.
        let found = MemStore::new()
            .exec(&call(&["search", "notes.sqlite", "oat milk"]), Some(&here))
            .await;
        assert_eq!(found.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&found.stdout),
            "User switched to oat milk\n"
        );
    }

    /// A store that is not there ends the line, and nothing is created on the way past.
    #[tokio::test]
    async fn insert_into_a_store_that_is_not_there_says_so() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let here = Here(dir.path().to_path_buf());

        let result = MemStore::new()
            .exec(
                &call(&["insert", "missing.sqlite", "User switched to oat milk"]),
                Some(&here),
            )
            .await;

        assert_eq!(result.exit_code, 1);
        assert!(result.stdout.is_empty(), "nothing to read as a memory");
        let said = String::from_utf8_lossy(&result.stderr);
        assert!(said.contains("missing.sqlite"), "{said}");
        assert!(
            !dir.path().join("missing.sqlite").exists(),
            "a failed insert made nothing"
        );
    }

    /// Nothing to write is a no-op and a zero — the answer a script with an empty list wants,
    /// rather than a case it has to check for before it can call this at all.
    #[tokio::test]
    async fn insert_with_nothing_to_write_writes_nothing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let here = Here(dir.path().to_path_buf());

        let made = MemStore::new()
            .exec(&call(&["init", "notes.sqlite"]), Some(&here))
            .await;
        assert_eq!(made.exit_code, 0);

        let result = MemStore::new()
            .exec(&call(&["insert", "notes.sqlite"]), Some(&here))
            .await;
        assert_eq!(
            result.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.stdout.is_empty(), "no memories, so no lines");
        assert!(result.stderr.is_empty(), "and nothing to explain");
    }

    /// Whitespace comes off, and what is nothing but whitespace is not a memory at all.
    ///
    /// A `2` and not a `1`: the argument was never usable, which is a line to reissue rather
    /// than work that was attempted and failed — and clap names which argument, which a check
    /// made later at the store could not.
    #[test]
    fn an_empty_memory_is_a_misunderstood_line() {
        let args::Command::Insert(insert) = parse(&["insert", "notes.sqlite", "  spaced  \n"])
            .expect("edges come off a memory")
        else {
            panic!("insert was asked for");
        };
        assert_eq!(insert.texts, ["spaced"]);

        for blank in ["", "   ", "\n\t "] {
            let e = parse(&["insert", "notes.sqlite", blank]).expect_err("a memory is a statement");
            assert_eq!(e.kind(), ErrorKind::ValueValidation);
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
    #[tokio::test]
    async fn search_answers_with_the_memories_nearest_the_question() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let here = Here(dir.path().to_path_buf());

        let made = MemStore::new()
            .exec(&call(&["init", "notes.sqlite"]), Some(&here))
            .await;
        assert_eq!(made.exit_code, 0);
        let written = MemStore::new()
            .exec(
                &call(&[
                    "insert",
                    "notes.sqlite",
                    "User drinks tea",
                    "User switched to oat milk",
                ]),
                Some(&here),
            )
            .await;
        assert_eq!(written.exit_code, 0);

        let answered = MemStore::new()
            .exec(&call(&["search", "notes.sqlite", "oat milk"]), Some(&here))
            .await;
        assert_eq!(
            answered.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&answered.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&answered.stdout),
            "User switched to oat milk\n"
        );

        // The bound reaches the store: two memories are near "user", and one line comes back.
        let one = MemStore::new()
            .exec(
                &call(&["search", "notes.sqlite", "user", "-n", "1"]),
                Some(&here),
            )
            .await;
        assert_eq!(one.exit_code, 0);
        assert_eq!(one.stdout.iter().filter(|b| **b == b'\n').count(), 1);

        // Nothing near the question is no lines and a zero: the store was read, and it holds
        // nothing near this.
        let nothing = MemStore::new()
            .exec(&call(&["search", "notes.sqlite", "almond"]), Some(&here))
            .await;
        assert_eq!(nothing.exit_code, 0);
        assert!(nothing.stdout.is_empty());
        assert!(nothing.stderr.is_empty());
    }

    /// A store that is not there is the answer, and the same one `insert` gives: a search that
    /// made an empty store would answer every question with nothing, forever.
    #[tokio::test]
    async fn searching_a_store_that_is_not_there_says_so() {
        let dir = tempfile::tempdir().expect("a temporary directory");

        let result = MemStore::new()
            .exec(
                &call(&["search", "missing.sqlite", "oat milk"]),
                Some(&Here(dir.path().to_path_buf())),
            )
            .await;

        assert_eq!(result.exit_code, 1);
        assert!(result.stdout.is_empty(), "nothing to read as a memory");
        let said = String::from_utf8_lossy(&result.stderr);
        assert!(said.contains("missing.sqlite"), "{said}");
        assert!(
            !dir.path().join("missing.sqlite").exists(),
            "searching made nothing"
        );
    }

    /// A search is a store and a question, and the count is the caller's to raise.
    #[test]
    fn a_search_is_bounded_whether_or_not_the_caller_says_so() {
        let args::Command::Search(search) =
            parse(&["search", "notes.sqlite", "oat milk"]).expect("a store and a question")
        else {
            panic!("search was asked for");
        };
        assert_eq!(search.query, "oat milk");
        assert_eq!(search.limit, 10);

        let args::Command::Search(search) =
            parse(&["search", "notes.sqlite", "oat milk", "-n", "3"]).expect("a shorter answer")
        else {
            panic!("search was asked for");
        };
        assert_eq!(search.limit, 3);
    }

    /// Nothing is mounted, so there is no store to open — and the command says that rather than
    /// resolving the name against something else.
    #[tokio::test]
    async fn with_nothing_mounted_there_is_no_store() {
        let result = MemStore::new()
            .exec(&call(&["search", "notes.sqlite", "oat milk"]), None)
            .await;
        assert_eq!(result.exit_code, 1);
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("nothing is mounted"),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    /// Help is an answer, not a refusal: stdout and a zero, so a caller can pipe it.
    #[tokio::test]
    async fn help_is_an_answer() {
        let result = MemStore::new().exec(&call(&["--help"]), None).await;
        assert_eq!(result.exit_code, 0);
        assert!(result.stderr.is_empty());
        let said = String::from_utf8_lossy(&result.stdout);
        for command in ["init", "insert", "search"] {
            assert!(said.contains(command), "{command} is missing from: {said}");
        }
    }
    /// The usage line is spelled with the name it was invoked by, so an executable registered
    /// under another name does not tell a caller to run a command they do not have.
    ///
    /// This is what `cortex_exec_storebase::command::as_registered` is for, and what clap's `string`
    /// feature is on for. Before the shared shell this crate baked its own name in, and a
    /// consumer registering it as anything else got usage for a command that did not exist.
    #[tokio::test]
    async fn usage_names_the_name_it_was_called_by() {
        let out = MemStore::new()
            .exec(
                &ExecCall {
                    name: "recall".into(),
                    args: vec!["--help".into()],
                    cwd: Some(String::new()),
                    env: Default::default(),
                },
                // Help is answered by the parser, before anything asks for a tree.
                None,
            )
            .await;

        assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(said.contains("Usage: recall"), "{said}");
        assert!(!said.contains("Usage: memstore"), "{said}");
    }

}
