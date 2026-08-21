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
/// on. `mem` is delegated in-process, so clap's usual job — read `argv`, print, exit — is exactly
/// what must not happen: the words come from an [`ExecCall`] and the answer goes back as bytes
/// and a code, on a session that has other work to do. What is being borrowed from clap is the
/// parsing and the help, which is to say the conventions a person expects of a command line —
/// `--flag=value`, `--`, a suggestion when they mistype an option — and not the driving.
mod args {
    use std::path::{Path, PathBuf};

    use ailoy::message::Message;
    use clap::Parser;

    use crate::lang::Lang;

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

        /// the language its memories are written in, as a language tag: en, ko-KR, zh-Hant-TW
        ///
        /// A property of the store and not of a call, which is why it is settled here: every
        /// later extraction writes in it, and a file whose memories are half in one language and
        /// half in another is one that answers a search well in neither.
        ///
        /// An option and not a positional, because the default is the answer almost every caller
        /// wants and a positional would make them type it to get to the arguments after it.
        ///
        /// A [`Lang`] and not a `String`, so that what reaches the store is a tag and not
        /// whatever was typed — see that module for what is accepted and why the answer is BCP
        /// 47 rather than a word. Parsed here, which is what makes a language nobody can read
        /// back a line that was *not understood*: exit `2` and clap naming the argument, rather
        /// than a store created and then labelled with a mystery.
        #[arg(long, default_value = "en", long_help = None,
              value_parser = |tag: &str| tag.parse::<Lang>())]
        pub lang: Lang,
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
    }
}

/// The `mem` command, as a name a console can delegate.
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
            // delegated call carries its words as `ExecCall::args`, which is `Vec<String>`, so
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
                args::Command::Init(init) => {
                    // The host path, because that is where a file is actually made — and the
                    // caller's name for it in everything said about it, since the host path is
                    // on the far side of a mount they cannot see.
                    match crate::store::Store::try_new(&store, &init.lang) {
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
                    // file and not from a provider they have already paid.
                    let opened = match crate::store::Store::try_from_file(&store) {
                        Ok(opened) => opened,
                        Err(e) => {
                            return ExecResult::failed(
                                1,
                                format!("mem: {}: {e}\n", store_path.display()),
                            );
                        }
                    };

                    // The language the memories are to be written in, and the store's rather
                    // than this conversation's: what a search over the file has to be able to
                    // assume is that everything in it is written one way. Asked before the model
                    // for the same reason the store was — a file that cannot say what language
                    // it holds is a store to fix, not one to spend a provider call on.
                    let lang = match opened.lang() {
                        Ok(lang) => lang,
                        Err(e) => {
                            return ExecResult::failed(
                                1,
                                format!("mem: {}: {e}\n", store_path.display()),
                            );
                        }
                    };

                    // Nothing offered as already held. What fills that slice is a search of the
                    // store for the memories nearest this conversation, and the store is the
                    // half that is not here yet — so every memory the extraction finds is new,
                    // which is the right answer for an empty store and a wasteful one for any
                    // other.
                    // `call.env` and not this process's: what pays for the call is the key the
                    // calling session had.
                    let memories =
                        match extractor::extract_memories(&insert.messages, &[], &lang, &call.env)
                            .await
                        {
                            Ok(memories) => memories,
                            // `1`: the line was understood and the work did not happen. `{e:#}` for
                            // the whole chain — which variable was unset, which model was asked,
                            // what came back — because the last link alone rarely says what to do.
                            Err(e) => return ExecResult::failed(1, format!("mem: {e:#}\n")),
                        };

                    // Reported and not kept. Writing these to the store is what remains, and
                    // until it is here the difference has to be *said*: a caller who read a
                    // zero exit and an empty store would have every reason to think `mem` had
                    // decided there was nothing worth remembering.
                    // One memory per line, which is what a memory being a statement makes
                    // possible: nothing here has to say which turn it came from or when,
                    // because a memory that needed either would not have been written.
                    let mut said = String::new();
                    for memory in &memories {
                        said.push_str(&memory.text);
                        said.push('\n');
                    }
                    ExecResult {
                        stdout: said.into_bytes(),
                        stderr: format!(
                            "mem: {} memories were found and none were written to {}: \
                             storing is not implemented yet\n",
                            memories.len(),
                            store.display()
                        )
                        .into_bytes(),
                        exit_code: 0,
                        timed_out: false,
                    }
                }
                args::Command::Search(search) => todo!(
                    "answer {:?} from the store at {}",
                    search.query,
                    store.display()
                ),
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
        let command = parse(&[
            "insert",
            "unused.sqlite",
            r#"{"role":"user","contents":[{"type":"text","text":
               "I switched from almond milk to oat milk after developing an almond sensitivity."}]}"#,
            r#"{"role":"assistant","contents":[{"type":"text","text":
               "Noted — I will keep that in mind for recipe suggestions."}]}"#,
        ])
        .expect("the conversation parses");
        let args::Command::Insert(insert) = command else {
            panic!("insert was asked for");
        };

        // What a delegated call is handed as `call.env`; here the program's own environment,
        // which is where `.env` just landed.
        let env = std::env::vars().collect();
        // Korean for an English conversation, which is the arrangement worth asking a real model
        // about: the language is the store's, so agreeing with it means disagreeing with every
        // message in front of it.
        let lang = "ko-KR".parse().expect("a language tag");
        let memories = extractor::extract_memories(&insert.messages, &[], &lang, &env)
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
    }

    /// A store is written in some language, and `en` is the one nobody has to say.
    #[test]
    fn a_store_is_made_for_a_language_and_english_is_assumed() {
        let args::Command::Init(init) =
            parse(&["init", "notes.sqlite"]).expect("a store is all init needs to be told")
        else {
            panic!("init was asked for");
        };
        assert_eq!(init.store, Path::new("notes.sqlite"));
        assert_eq!(init.lang.as_str(), "en");

        let args::Command::Init(init) =
            parse(&["init", "notes.sqlite", "--lang", " ko_kr "]).expect("a language can be named")
        else {
            panic!("init was asked for");
        };
        assert_eq!(
            init.lang.as_str(),
            "ko-KR",
            "what the store is labelled with is the tag, not the typing"
        );
    }

    /// A language nobody can read back is a line that was *not understood* — refused where the
    /// arguments are read, so no store is made to hold a label that means nothing.
    #[test]
    fn a_language_that_is_not_a_tag_is_a_misunderstood_line() {
        for written in ["   ", "Korean", "ko-KRR"] {
            let e = parse(&["init", "notes.sqlite", "--lang", written])
                .expect_err(&format!("`{written}` is not a language tag"));
            assert_eq!(e.kind(), ErrorKind::ValueValidation, "`{written}`");
        }
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
            .exec(
                &call(&["init", "notes.sqlite", "--lang", "ko"]),
                Some(&here),
            )
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
