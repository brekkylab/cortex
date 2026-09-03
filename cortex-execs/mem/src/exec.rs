//! `mem` as a command: arguments in, lines out.

use clap::{Parser as _, error::ErrorKind};
use cortex::{
    BoxFuture,
    exec::{ExecCall, ExecResult, Executable},
    fs::Mount,
};
use futures::FutureExt;

use crate::extractor;

/// The command line, as declarations — what `mem` accepts and what it says about it.
///
/// Only shapes and their help live here; the reading of a line happens where the line is acted
/// on. `mem` runs in-process, so clap's usual job — read `argv`, print, exit — is exactly
/// what must not happen: the words come from an [`ExecCall`] and the answer goes back as bytes
/// and a code, on a session that has other work to do. What is being borrowed from clap is the
/// parsing and the help, which is to say the conventions a person expects of a command line —
/// `--flag=value`, `--`, a suggestion when they mistype an option — and not the driving.
mod args {
    use std::path::{Path, PathBuf};

    use ailoy::message::Message;
    use clap::Parser;

    #[derive(Debug, Parser)]
    #[command(
        name = "mem",
        about = "mem — a memory store that is one file in this tree",
        after_help = "<store> is a path in this tree, resolved like any other argument. `init` creates it; every other command expects it to be there already.",
        // The doc comments in this module are for whoever reads the module. What a caller of the
        // command sees is `about` and the first line of each item's; nothing below argues clap's
        // design decisions at somebody who typed `--help`.
        long_about = None,
        // Bare `mem` is a line that asked for nothing, and the help is the useful answer to it.
        arg_required_else_help = true,
        // `mem help insert` as well as `mem insert --help` is a second spelling of one thing.
        disable_help_subcommand = true,
        // Nothing here is versioned on its own: this ships as part of a console, not as a program
        // somebody installed and might hold an old copy of.
        disable_version_flag = true
    )]
    pub struct Cli {
        #[command(subcommand)]
        pub command: Command,
    }

    /// What a command line asked for.
    ///
    /// No equality: one of these holds a conversation, and what it would mean for two
    /// conversations to be equal is a question this crate never asks. A derive kept for the
    /// look of it would have to be answered — by comparing serialised JSON, say — and that is
    /// a definition of sameness invented for a caller that does not exist.
    #[derive(Clone, Debug, clap::Subcommand)]
    pub enum Command {
        /// make a store at <store>
        ///
        /// The one command that brings a store into being. Every other one is about memories
        /// and expects the file to be there, which is what keeps a mistyped name from becoming
        /// an empty store that answers every search with nothing.
        #[command(long_about = None)]
        Init(Init),

        /// remember what is worth remembering in <messages>
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

    /// `mem init <store>` — a store where there was no file.
    #[derive(Clone, Debug, clap::Args)]
    pub struct Init {
        /// the store to make, as a path in this tree
        ///
        /// A path and not a name: what this ends up as is a file, and typing it as one is what
        /// keeps a caller from being handed a `String` that has to be remembered to be a path
        /// three functions later.
        #[arg(long_help = None)]
        pub store: PathBuf,
    }

    /// `mem insert <store> <messages>` — a conversation, read for what is worth keeping.
    #[derive(Clone, Debug, clap::Args)]
    pub struct Insert {
        /// the store to write to, as a path in this tree
        ///
        /// A path and not a name: what this ends up as is a file, and typing it as one is what
        /// keeps a caller from being handed a `String` that has to be remembered to be a path
        /// three functions later.
        #[arg(long_help = None)]
        pub store: PathBuf,

        /// the conversation to remember: one JSON message per argument
        ///
        /// A conversation and not a sentence, because that is where memories actually are: the
        /// fact is in one turn, the date it happened in the turn before, and the pronoun naming
        /// it in the turn after. Whatever arrives is data — nothing here shortens or tidies it,
        /// since deciding what matters is the extraction's job and not the parser's.
        ///
        /// A turn per argument, rather than one argument holding a JSON list of them. A command
        /// line is already a list of strings, and `ExecCall::args` hands it over as one — so
        /// spelling the turns as separate arguments lets argv carry the list, where a JSON array
        /// inside a single argument would be a second list nested in the first. It also keeps
        /// each turn separately quoted, which is the difference between a shell line a person can
        /// read and one enormous argument.
        #[arg(long_help = None, value_parser = |text: &str| serde_json::from_str::<Message>(text)
            .map_err(|e| format!("cannot parse to a message: {e}")))]
        pub messages: Vec<Message>,
    }

    /// `mem search <store> <query>` — the memories in one store nearest what was asked.
    #[derive(Clone, Debug, clap::Args)]
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
        ///
        /// Ten because both readers of this want about that many: a person reads a screen, and
        /// an `insert` offering the neighbourhood to an extraction pays per memory it offers.
        #[arg(short = 'n', long, default_value_t = 10, long_help = None)]
        pub limit: usize,
    }
}

/// How many held memories an `insert` shows the extraction.
///
/// Not a bound on an answer — nobody reads these — but on what one call is willing to spend on
/// not repeating itself. The two costs are asymmetric, which is what settles the number: a
/// memory left out of the neighbourhood is offered as new and becomes a duplicate row that
/// nothing later retires, where a memory included needlessly is a sentence of prompt on one
/// call. So this errs high, and higher than the ten a person is shown by `search`.
///
/// It is a constant and not a flag because the caller it would be a flag for does not exist: an
/// `insert` is issued by a session that has just finished talking, and how many memories the
/// extraction needs to see in order not to repeat one is a fact about the extraction.
const NEIGHBOURHOOD: usize = 20;

/// The `mem` command, as an [`Executable`](cortex::exec::Executable).
#[derive(Debug, Clone, Default)]
pub struct Mem {}

impl Mem {
    pub fn new() -> Self {
        Self {}
    }
}

/// Delegated `mem`, answering on stdout and stderr like the program it stands in for.
///
/// The whole command is here, in the one method the trait has: a line is read, the store it names
/// is found, and the work is done, in that order and in that order only. Every way it can end is
/// an [`ExecResult`] written where it is decided, because what a reader wants to know about an
/// exit code is which line produced it.
impl Executable for Mem {
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecResult> {
        async move {
            // The name back on the front: clap reads `argv`, whose first word is the program, and
            // what arrives here is what a caller wrote *after* it. Prepending `mem` rather than
            // suppressing the name is what puts it in front of every usage line clap renders.
            let argv = std::iter::once("mem").chain(call.args.iter().map(String::as_str));

            let command = match args::Cli::try_parse_from(argv) {
                Ok(cli) => cli.command,
                // Help is what was asked for, so it is an answer and not a refusal: stdout, and a
                // zero exit for the caller that piped it somewhere.
                Err(e) if e.kind() == ErrorKind::DisplayHelp => {
                    return ExecResult::ok(e.to_string());
                }
                // `2` and not `1`, because a caller can act on the difference: this is a line that
                // was not understood and can be reissued differently, where `1` is one that was
                // understood and did not work. Both say why on stderr and put nothing on stdout,
                // so a caller reading output never reads an explanation as data.
                Err(e) => return ExecResult::failed(2, e.to_string()),
            };

            // What makes the mount necessary rather than convenient: `notes.sqlite` is a name in
            // the *workspace*, and a store is opened by host path — so the name is resolved
            // against where the calling command stood and then through the mount, which is the
            // same file the caller would have seen. With nothing mounted there is no such file and
            // no honest substitute for one, so the command refuses.
            let Some(mount) = mount else {
                return ExecResult::failed(
                    1,
                    "mem: nothing is mounted, so there is no store to open\n",
                );
            };

            // Every command names a store, so finding it happens once, before they part ways:
            // what differs between them is what they then do to it. Owned, because the command
            // is taken apart below and this name outlives it: it is what every message about the
            // store says, including the ones the work itself produces.
            let store_path = command.get_store_path().to_path_buf();

            // The `expect` is the argument's own history, not an assumption about paths: a
            // call carries its words as `ExecCall::args`, which is `Vec<String>`, so
            // every byte in this path was UTF-8 before clap ever spelled it as one. A workspace
            // path is text at both ends — `cwd` is a `String`, and so is what `resolve` reads —
            // and the only reason it is a `PathBuf` in between is that a path is what it names.
            let store = match call.resolve(
                store_path
                    .to_str()
                    .expect("a store named on a command line came from a `String`"),
            ) {
                Ok(path) => mount.host_path(&path),
                // Named as the caller named it: the host path is on the far side of a mount they
                // cannot see, and would not tell them which store was meant.
                Err(e) => {
                    return ExecResult::failed(1, format!("mem: {}: {e}\n", store_path.display()));
                }
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
                                    "mem: {}: there is already a file here; \
                                     `mem insert` writes to a store that exists\n",
                                    store_path.display()
                                ),
                            )
                        }
                        Err(e) => {
                            ExecResult::failed(1, format!("mem: {}: {e}\n", store_path.display()))
                        }
                    }
                }
                args::Command::Insert(insert) => {
                    // The store first, and before the model: `insert` writes to a store that
                    // exists, and a caller who named the wrong file should learn it from the
                    // file and not from a provider they have already paid. Held open across the
                    // extraction rather than reopened after it — a connection is not a lock, and
                    // what the write needs is the file that was checked and not another one that
                    // has the same name by the time the model answers.
                    let store = match crate::store::Store::try_from_file(&store) {
                        Ok(store) => store,
                        Err(e) => {
                            return ExecResult::failed(
                                1,
                                format!("mem: {}: {e}\n", store_path.display()),
                            );
                        }
                    };

                    // What the store already holds near this conversation, so that the
                    // extraction can decline to restate it. Nothing here reconciles anything —
                    // the neighbourhood is shown and never written back — but without it every
                    // insert offers every fact it finds as new, and a store told the same thing
                    // twice holds it twice with nothing to tell the copies apart. That is the
                    // one kind of damage this command can do that no later command undoes.
                    //
                    // Failing rather than carrying on with an empty neighbourhood: a store that
                    // cannot be read is about to be written to, and the honest thing to do about
                    // a half-working store is not to add rows to it.
                    let existing =
                        match store.search(&extractor::as_query(&insert.messages), NEIGHBOURHOOD) {
                            Ok(existing) => existing,
                            Err(e) => {
                                return ExecResult::failed(
                                    1,
                                    format!("mem: {}: {e}\n", store_path.display()),
                                );
                            }
                        };

                    // `call.env` and not this process's: what pays for the call is the key the
                    // calling session had.
                    let memories =
                        match extractor::extract_memories(&insert.messages, &existing, &call.env)
                            .await
                        {
                            Ok(memories) => memories,
                            // `1`: the line was understood and the work did not happen. `{e:#}` for
                            // the whole chain — which variable was unset, which model was asked,
                            // what came back — because the last link alone rarely says what to do.
                            Err(e) => return ExecResult::failed(1, format!("mem: {e:#}\n")),
                        };

                    // Written before anything is said about them, so that what a caller reads on
                    // stdout is what the store holds and not what a model answered: a write that
                    // fails after the lines were printed would have told them the opposite of
                    // what happened.
                    if let Err(e) = store.insert(&memories) {
                        return ExecResult::failed(
                            1,
                            format!("mem: {}: {e}\n", store_path.display()),
                        );
                    }

                    // One memory per line, which is what a memory being a statement makes
                    // possible: nothing here has to say which turn it came from or when,
                    // because a memory that needed either would not have been written. Nothing
                    // worth remembering is therefore no output at all — the honest answer for a
                    // conversation that carried none, and the one a script reading lines wants.
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
                        Err(e) => {
                            return ExecResult::failed(
                                1,
                                format!("mem: {}: {e}\n", store_path.display()),
                            );
                        }
                    };

                    let found = match store.search(&search.query, search.limit) {
                        Ok(found) => found,
                        Err(e) => {
                            return ExecResult::failed(
                                1,
                                format!("mem: {}: {e}\n", store_path.display()),
                            );
                        }
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
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use clap::{Parser as _, error::ErrorKind};

    use super::*;

    fn parse(line: &[&str]) -> Result<args::Command, clap::Error> {
        args::Cli::try_parse_from(std::iter::once("mem").chain(line.iter().copied()))
            .map(|cli| cli.command)
    }

    /// The whole path with a model actually asked. The key comes from `.env`, searched from
    /// this package upward; anything already exported wins over the file.
    ///
    /// ```text
    /// cargo test -p cortex-exec-mem-v2 memory_can_be_extracted -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "asks a real provider: needs a key, a network, and spends money"]
    async fn memory_can_be_extracted() {
        dotenvy::dotenv().ok();

        // Through the command line, so this covers what a caller types. The store is named and
        // unused: the extraction never looks at it.
        //
        // Korean, and deliberately carrying no name, title or quoted phrase — those are kept as
        // they were written, so a conversation with one in it could not tell a memory written in
        // English from a memory written in Korean.
        let command = parse(&[
            "insert",
            "unused.sqlite",
            r#"{"role":"user","contents":[{"type":"text","text":
               "아몬드 알레르기가 생겨서 아몬드 우유에서 오트 우유로 바꿨어요."}]}"#,
            r#"{"role":"assistant","contents":[{"type":"text","text":
               "알겠습니다 — 레시피를 추천할 때 참고하겠습니다."}]}"#,
        ])
        .expect("the conversation parses");
        let args::Command::Insert(insert) = command else {
            panic!("insert was asked for");
        };

        // What a call is handed as `call.env`; here the program's own environment,
        // which is where `.env` just landed.
        let env = std::env::vars().collect();
        let memories = extractor::extract_memories(&insert.messages, &[], &env)
            .await
            .expect("the extraction answers");

        for memory in &memories {
            println!("{}", memory.text);
        }

        // What can honestly be asserted about text a model wrote: that it is there, and that
        // the one unmissable fact in the conversation survived.
        assert!(!memories.is_empty(), "the conversation carries a fact");
        assert!(
            memories.iter().all(|m| !m.text.trim().is_empty()),
            "an empty memory is not a memory"
        );
        assert!(
            memories
                .iter()
                .any(|m| m.text.to_lowercase().contains("oat")),
            "the switch to oat milk is what this conversation is about: {memories:#?}"
        );
        let hangul = |text: &str| {
            text.chars().any(|c| {
                matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}')
            })
        };
        assert!(
            !memories.iter().any(|m| hangul(&m.text)),
            "the conversation is Korean and the memories are English: {memories:#?}"
        );
    }

    /// The same conversation twice, through the whole command both times. The second `insert`
    /// is shown what the first wrote and has nothing left to add — which is the only way to see
    /// that the neighbourhood reaches the extraction at all, since everything between the
    /// search and the answer happens inside a model.
    ///
    /// Ignored for the reason the test above is, and worth running whenever anything on the
    /// path between [`Store::search`](crate::store::Store::search) and the prompt changes: a
    /// store that quietly stopped offering what it holds looks exactly like one that works,
    /// until it has two copies of everything.
    ///
    /// ```text
    /// cargo test -p cortex-exec-mem a_conversation_inserted_twice -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "asks a real provider: needs a key, a network, and spends money"]
    async fn a_conversation_inserted_twice_is_not_remembered_twice() {
        dotenvy::dotenv().ok();

        let dir = tempfile::tempdir().expect("a temporary directory");
        struct Here(PathBuf);
        impl Mount for Here {
            fn mountpoint(&self) -> &Path {
                &self.0
            }
        }
        let here = Here(dir.path().to_path_buf());

        let env: std::collections::BTreeMap<_, _> = std::env::vars().collect();
        let call = |args: &[&str]| ExecCall {
            name: "mem".into(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            cwd: Some(String::new()),
            env: env.clone(),
        };

        let made = Mem::new()
            .exec(&call(&["init", "notes.sqlite"]), Some(&here))
            .await;
        assert_eq!(
            made.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );

        let conversation = [
            "insert",
            "notes.sqlite",
            r#"{"role":"user","contents":[{"type":"text","text":
               "I switched from almond milk to oat milk because I developed an almond allergy."}]}"#,
            r#"{"role":"assistant","contents":[{"type":"text","text":
               "Noted — I will keep that in mind when suggesting recipes."}]}"#,
        ];

        let first = Mem::new().exec(&call(&conversation), Some(&here)).await;
        assert_eq!(
            first.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        let first = String::from_utf8_lossy(&first.stdout);
        println!("--- first insert ---\n{first}");
        assert!(!first.trim().is_empty(), "the conversation carries a fact");

        let again = Mem::new().exec(&call(&conversation), Some(&here)).await;
        assert_eq!(
            again.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&again.stderr)
        );
        let again = String::from_utf8_lossy(&again.stdout);
        println!("--- second insert ---\n{again}");

        // What can honestly be asserted about a model's judgement: not that it writes nothing,
        // but that it does not write the store over again. A fact phrased a second way is one
        // line; a store that was never shown its own contents answers with all of them.
        let lines = |said: &str| said.lines().filter(|l| !l.trim().is_empty()).count();
        assert!(
            lines(&again) < lines(&first),
            "the second reading was shown what the first wrote:\n{again}"
        );
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

        // The tree as the `mem` binary supplies it: a directory on this host, with nothing
        // mounted over it.
        struct Here(PathBuf);
        impl Mount for Here {
            fn mountpoint(&self) -> &Path {
                &self.0
            }
        }
        let here = Here(dir.path().to_path_buf());

        let call = |args: &[&str]| ExecCall {
            name: "mem".into(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            cwd: Some(String::new()),
            env: Default::default(),
        };

        let made = Mem::new()
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

        let again = Mem::new()
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

    /// A store that is not there ends the line before a provider is asked anything — which is
    /// also why this test needs no key and no network.
    #[tokio::test]
    async fn insert_into_a_store_that_is_not_there_asks_no_model() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        struct Here(PathBuf);
        impl Mount for Here {
            fn mountpoint(&self) -> &Path {
                &self.0
            }
        }

        let call = ExecCall {
            name: "mem".into(),
            args: [
                "insert",
                "missing.sqlite",
                r#"{"role":"user","contents":[{"type":"text","text":"I switched to oat milk"}]}"#,
            ]
            .iter()
            .map(|a| (*a).to_string())
            .collect(),
            cwd: Some(String::new()),
            // No key in it, so a line that reached the extraction would fail saying so. That
            // this one does not is the whole assertion.
            env: Default::default(),
        };

        let result = Mem::new()
            .exec(&call, Some(&Here(dir.path().to_path_buf())))
            .await;
        assert_eq!(result.exit_code, 1);
        let said = String::from_utf8_lossy(&result.stderr);
        assert!(said.contains("missing.sqlite"), "{said}");
        assert!(
            !said.contains("API_KEY"),
            "the store was the answer, and no model was asked: {said}"
        );
    }

    /// The whole line with the model taken out of it: a store that exists, a conversation with
    /// nothing readable in it, and therefore nothing to write. It is the one path through
    /// `insert` that reaches the store without asking a provider anything, which is what makes
    /// it the test that the two halves are wired together at all — a zero exit here says the
    /// store was opened and written to, since either failing is a `1`.
    #[tokio::test]
    async fn insert_writes_to_the_store_it_was_given() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        struct Here(PathBuf);
        impl Mount for Here {
            fn mountpoint(&self) -> &Path {
                &self.0
            }
        }
        let here = Here(dir.path().to_path_buf());

        let made = Mem::new()
            .exec(
                &ExecCall {
                    name: "mem".into(),
                    args: ["init", "notes.sqlite"].map(String::from).into(),
                    cwd: Some(String::new()),
                    env: Default::default(),
                },
                Some(&here),
            )
            .await;
        assert_eq!(made.exit_code, 0);

        // A system turn is nobody speaking, so there is nothing here for an extraction to read
        // and no model is asked — which is also why this test needs no key.
        let result = Mem::new()
            .exec(
                &ExecCall {
                    name: "mem".into(),
                    args: [
                        "insert",
                        "notes.sqlite",
                        r#"{"role":"system","contents":[{"type":"text","text":"Be helpful."}]}"#,
                    ]
                    .map(String::from)
                    .into(),
                    cwd: Some(String::new()),
                    env: Default::default(),
                },
                Some(&here),
            )
            .await;

        assert_eq!(
            result.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            result.stdout.is_empty(),
            "nothing was remembered, so there is no line saying one was: {}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert!(
            result.stderr.is_empty(),
            "and nothing to explain: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    /// `search` end to end, as a caller types it: memories in a store, a question asked of it,
    /// and the answer on stdout nearest first. The store is written through [`crate::store`]
    /// rather than through `mem insert`, because what a memory *is* is settled by then — going
    /// through the extraction would put a model in the middle of a test about lines of output.
    #[tokio::test]
    async fn search_answers_with_the_memories_nearest_the_question() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        struct Here(PathBuf);
        impl Mount for Here {
            fn mountpoint(&self) -> &Path {
                &self.0
            }
        }
        let here = Here(dir.path().to_path_buf());

        let store = crate::store::Store::try_new(dir.path().join("notes.sqlite"))
            .expect("a store can be made");
        store
            .insert(&[
                crate::memory::Memory {
                    text: "User drinks tea".into(),
                },
                crate::memory::Memory {
                    text: "User switched to oat milk".into(),
                },
            ])
            .expect("the memories are written");
        drop(store);

        let ask = |args: &[&str]| ExecCall {
            name: "mem".into(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            cwd: Some(String::new()),
            env: Default::default(),
        };

        let answered = Mem::new()
            .exec(&ask(&["search", "notes.sqlite", "oat milk"]), Some(&here))
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
        let one = Mem::new()
            .exec(
                &ask(&["search", "notes.sqlite", "user", "-n", "1"]),
                Some(&here),
            )
            .await;
        assert_eq!(one.exit_code, 0);
        assert_eq!(one.stdout.iter().filter(|b| **b == b'\n').count(), 1);

        // Nothing near the question is no lines and a zero: the store was read, and it holds
        // nothing near this.
        let nothing = Mem::new()
            .exec(&ask(&["search", "notes.sqlite", "almond"]), Some(&here))
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
        struct Here(PathBuf);
        impl Mount for Here {
            fn mountpoint(&self) -> &Path {
                &self.0
            }
        }

        let result = Mem::new()
            .exec(
                &ExecCall {
                    name: "mem".into(),
                    args: ["search", "missing.sqlite", "oat milk"]
                        .map(String::from)
                        .into(),
                    cwd: Some(String::new()),
                    env: Default::default(),
                },
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

    /// A turn per argument, in the order they were written: argv carries the list.
    #[test]
    fn a_conversation_arrives_as_messages() {
        let command = parse(&[
            "insert",
            "notes.sqlite",
            r#"{"role":"user","contents":[{"type":"text","text":"I switched to oat milk"}]}"#,
            r#"{"role":"assistant","contents":[{"type":"text","text":"Noted."}]}"#,
        ])
        .expect("a turn per argument is what insert takes");

        let args::Command::Insert(insert) = command else {
            panic!("insert was asked for");
        };
        let messages = &insert.messages;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, ailoy::message::Role::User);
        assert_eq!(
            messages[0].contents[0].as_text(),
            Some("I switched to oat milk")
        );
        assert_eq!(messages[1].role, ailoy::message::Role::Assistant);
    }

    /// The whole reason the parse lives in clap: a turn that will not parse is a line that was
    /// *not understood*, and [`Executable::exec`] leaves with `2` for those and `1` for a line
    /// it understood and could not carry out. Reading the text later would have made this the
    /// second kind — and would have made a caller resolve a mount first, on behalf of an
    /// argument that was never usable.
    #[test]
    fn a_turn_that_will_not_parse_is_a_misunderstood_line() {
        let e =
            parse(&["insert", "notes.sqlite", "not json at all"]).expect_err("prose is not a turn");
        assert_eq!(e.kind(), ErrorKind::ValueValidation);

        // What a caller needs off this line, rather than the sentence it is currently said in:
        // which argument was wrong, and where in it. Pinning the wording would make every
        // rephrasing a failing test, and the wording is not what anybody depends on.
        let said = e.to_string();
        assert!(
            said.contains("MESSAGES"),
            "clap names the argument that was wrong, which a parse done later could not: {said}"
        );
        assert!(
            said.contains("line 1 column"),
            "serde's half says where in the value it gave up: {said}"
        );
    }

    /// JSON of the wrong shape is the same failure as no JSON at all — the argument's type is
    /// [`Message`](ailoy::message::Message), not "some JSON".
    #[test]
    fn json_that_is_not_a_message_is_not_a_turn() {
        let e = parse(&["insert", "notes.sqlite", r#"{"role":"user"}"#])
            .expect_err("a role with nothing said is not a message");
        assert_eq!(e.kind(), ErrorKind::ValueValidation);

        let e = parse(&["insert", "notes.sqlite", r#""hello""#])
            .expect_err("a string is not a message");
        assert_eq!(e.kind(), ErrorKind::ValueValidation);
    }

    /// One bad turn fails the line, rather than being dropped from a conversation that then
    /// looks complete: a store written from what was left would be missing a turn nobody was
    /// told about.
    #[test]
    fn one_unreadable_turn_fails_the_whole_line() {
        let e = parse(&[
            "insert",
            "notes.sqlite",
            r#"{"role":"user","contents":[{"type":"text","text":"hello"}]}"#,
            "{ not a message }",
        ])
        .expect_err("the second turn does not parse");
        assert_eq!(e.kind(), ErrorKind::ValueValidation);
    }

    /// Nothing said is a conversation too, and one this command is willing to be handed: that
    /// it holds no memories is for the extraction to answer, not the parser. A script that
    /// happens to have no turns to offer gets a no-op and a zero, where a required argument
    /// would make it special-case the empty case itself.
    #[test]
    fn a_conversation_with_no_turns_still_parses() {
        let command = parse(&["insert", "notes.sqlite"]).expect("no turns is a conversation");
        let args::Command::Insert(insert) = command else {
            panic!("insert was asked for");
        };
        assert!(insert.messages.is_empty());
    }
}
