//! Ingest a tree through a mount, then find it again — all through the one `docstore` name.
//!
//! The mount here is a plain directory, which is what a [`Mount`] is from this executable's
//! side: a path any process can `open`. Standing one up for real needs a binding, a libfuse
//! provider and a kernel, and none of that is what these test.
//!
//! What these pin is the observable contract: what a walk picks up, what a second `ingest`
//! means, what `sync` removes, and which stream each answer goes to. A test whose property is a
//! consequence of the store being a file in the tree (every command needs the mount) says so in
//! its own name.

use std::path::{Path, PathBuf};

use cortex::exec::{ExecCall, ExecResult, ExecutableSet};
use cortex::fs::Mount;
use cortex_exec_docstore::DocStore;

struct Mounted(PathBuf);

impl Mount for Mounted {
    fn mountpoint(&self) -> &Path {
        &self.0
    }
}

/// The set as a consumer registers it. There is no root to hand over: a store is a path in the
/// tree, so the executable holds nothing.
fn registered() -> ExecutableSet {
    ExecutableSet::new().register("docstore", DocStore::SUMMARY, DocStore::new())
}

/// A tree to index. The store goes in it too, which is the arrangement being tested.
fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temp dir");
    let at = |rel: &str, text: &str| {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    at(
        "docs/ownership.md",
        "Rust ownership is what makes the borrow checker possible.\n",
    );
    at("docs/mounts.md", "A mount is a path the kernel answers on.\n");
    at(
        "docs/sub/nested.md",
        "Nested prose about lifetimes and elision.\n",
    );
    // Not in the allowlist, so a walk should leave it alone.
    std::fs::write(dir.path().join("docs/ignore.bin"), [0xff, 0x00]).unwrap();
    // Dotted, so the walk should not descend into it.
    at("docs/.hidden/secret.md", "Nobody should index this.\n");
    // A sibling whose name starts with the same letters as `docs`.
    at("docs-other/x.md", "The borrow checker elsewhere entirely.\n");
    dir
}

fn call(args: &[&str]) -> ExecCall {
    ExecCall {
        name: "docstore".into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: Default::default(),
        // `None` — a backend that reports no working directory, so every path below is
        // workspace-absolute and the test is written the way a caller has to write it then.
        cwd: None,
    }
}

/// Run one line against `tree`, and insist it was understood.
async fn run(tree: &Path, args: &[&str]) -> ExecResult {
    registered()
        .invoke(&call(args), Some(&Mounted(tree.to_path_buf())))
        .await
        .expect("docstore is registered")
}

/// Run one line and insist it succeeded, handing back stdout.
async fn ok(tree: &Path, args: &[&str]) -> String {
    let out = run(tree, args).await;
    assert_eq!(
        out.exit_code,
        0,
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A tree with a store in it, already made.
async fn ready() -> tempfile::TempDir {
    let dir = tree();
    ok(dir.path(), &["init", "/notes.db"]).await;
    dir
}

/// The paths a search answered with, in the order it answered them.
fn hits(said: &str) -> Vec<String> {
    said.lines()
        .filter(|line| !line.starts_with('\t'))
        .filter_map(|line| line.split('\t').nth(1).map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn ingest_then_search_finds_the_file_by_its_words() {
    let dir = ready().await;
    assert_eq!(
        ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await,
        "indexed 3 file(s)\n",
        "the allowlist took the three .md files and nothing else"
    );

    let said = ok(dir.path(), &["search", "/notes.db", "ownership"]).await;
    assert_eq!(hits(&said), ["docs/ownership.md"]);
    // The snippet is the line under the hit, cut around what was asked for.
    assert!(
        said.lines().nth(1).is_some_and(|l| l.contains("ownership")),
        "{said}"
    );
}

/// The property that lets an agent run `ingest` without keeping track of what it has run.
#[tokio::test]
async fn ingesting_twice_means_the_same_as_ingesting_once() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;

    assert_eq!(ok(dir.path(), &["list", "/"]).await, "notes.db\t3 document(s)\n");
    let said = ok(dir.path(), &["search", "/notes.db", "ownership"]).await;
    assert_eq!(hits(&said), ["docs/ownership.md"], "one document, not two");
}

/// Overlapping arguments walk the same file twice; the count must not say so.
#[tokio::test]
async fn overlapping_arguments_are_counted_once() {
    let dir = ready().await;
    assert_eq!(
        ok(dir.path(), &["ingest", "/notes.db", "/docs", "/docs/sub"]).await,
        "indexed 3 file(s)\n"
    );
}

/// The walk's rules: an unlisted extension and a dotted directory are not in the store.
#[tokio::test]
async fn the_walk_skips_dotfiles_and_unlisted_extensions() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;

    let nothing = ok(dir.path(), &["search", "/notes.db", "Nobody"]).await;
    assert_eq!(nothing, "no matches\n", "the dotted directory was not walked");
    assert_eq!(ok(dir.path(), &["list", "/"]).await, "notes.db\t3 document(s)\n");
}

/// The bytes were read when they were ingested, so a search does not open the corpus again.
#[tokio::test]
async fn search_reads_no_file_of_the_corpus() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;
    std::fs::remove_dir_all(dir.path().join("docs")).unwrap();

    let said = ok(dir.path(), &["search", "/notes.db", "ownership"]).await;
    assert_eq!(hits(&said), ["docs/ownership.md"]);
}

/// Nothing near the question is no hits and a zero: the store was read and holds nothing near
/// this, which is an answer and not a failure.
#[tokio::test]
async fn a_question_with_no_answer_is_still_an_answer() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;

    let out = run(dir.path(), &["search", "/notes.db", "zygote"]).await;
    assert_eq!(out.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "no matches\n");

    // And a query with no words in it at all, which never reaches the index.
    let out = run(dir.path(), &["search", "/notes.db", "!!!"]).await;
    assert_eq!(out.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "no matches\n");
}

/// FTS5 reads its pattern as an expression, and a caller was never told that language.
#[tokio::test]
async fn a_query_that_looks_like_an_expression_is_read_as_words() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;

    for query in ["NOT", "ownership AND", "\"unbalanced", "a:b", "*", "(", "^x", "a OR"] {
        let out = run(dir.path(), &["search", "/notes.db", query]).await;
        assert_eq!(
            out.exit_code,
            0,
            "{query:?} was read as syntax: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // A word starting with `-` is the parser's business and not FTS5's: clap takes it for an
    // option, and `--` is how a caller says it is a word. That is the ordinary command-line
    // convention, and borrowing it is the reason there is a parser here at all.
    let flagged = run(dir.path(), &["search", "/notes.db", "-x"]).await;
    assert_eq!(flagged.exit_code, 2, "an unknown option is a misunderstood line");
    let quoted = run(dir.path(), &["search", "/notes.db", "--", "-x"]).await;
    assert_eq!(
        quoted.exit_code,
        0,
        "{}",
        String::from_utf8_lossy(&quoted.stderr)
    );
}

/// What indexing the body rather than a list of terms buys: `porter` stems, so a question asked
/// in one form finds a file written in another.
#[tokio::test]
async fn a_file_is_found_by_a_word_in_another_form() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;

    for asked in ["mount", "mounts", "mounting", "answer", "answers"] {
        assert_eq!(
            hits(&ok(dir.path(), &["search", "/notes.db", asked]).await),
            ["docs/mounts.md"],
            "{asked} did not reach the file"
        );
    }
}

/// The snippet is cut from the body as it was written — the store holds the original, which is
/// what lets the index be rebuilt under another tokenizer later without a re-ingest.
#[tokio::test]
async fn the_snippet_is_the_body_as_it_was_written() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;

    let said = ok(dir.path(), &["search", "/notes.db", "kernel"]).await;
    let snippet = said.lines().nth(1).expect("a snippet under the hit");
    assert!(
        snippet.contains("A mount is a path the kernel answers on"),
        "case and wording survive: {snippet}"
    );
}

/// A store that was never made is an error, not an empty index answering "no matches" — which
/// reads as a corpus with nothing in it.
#[tokio::test]
async fn a_store_that_is_not_there_is_not_made_by_using_it() {
    let dir = tree();
    for args in [
        vec!["search", "/nope.db", "ownership"],
        vec!["ingest", "/nope.db", "/docs"],
        vec!["purge", "/nope.db", "/docs"],
    ] {
        let out = run(dir.path(), &args).await;
        assert_eq!(out.exit_code, 1, "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?} wrote something to read");
        assert!(!dir.path().join("nope.db").exists(), "{args:?} made a store");
    }
}

#[tokio::test]
async fn init_will_not_make_a_store_twice() {
    let dir = ready().await;
    let again = run(dir.path(), &["init", "/notes.db"]).await;
    assert_eq!(again.exit_code, 1);
    assert!(again.stdout.is_empty(), "nothing to read as a store's path");
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("notes.db"),
        "the store is named as the caller named it"
    );
}

/// `purge` is `ingest` undone, and a sibling whose name merely starts the same way is not under
/// the path given.
#[tokio::test]
async fn purge_takes_a_subtree_out_and_leaves_a_sibling() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs", "/docs-other"]).await;
    assert_eq!(ok(dir.path(), &["list", "/"]).await, "notes.db\t4 document(s)\n");

    assert_eq!(
        ok(dir.path(), &["purge", "/notes.db", "/docs"]).await,
        "purged 3 document(s)\n"
    );
    let said = ok(dir.path(), &["search", "/notes.db", "borrow"]).await;
    assert_eq!(
        hits(&said),
        ["docs-other/x.md"],
        "`docs-other` is not under `docs`"
    );
}

/// The case `purge` exists for: the file is gone from the tree, and the document is not.
#[tokio::test]
async fn purge_opens_no_file_of_the_corpus() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;
    std::fs::remove_dir_all(dir.path().join("docs")).unwrap();

    assert_eq!(
        ok(dir.path(), &["purge", "/notes.db", "/docs"]).await,
        "purged 3 document(s)\n"
    );
}

/// The gap a second `ingest` leaves: added and changed files it handles, a removed one it does
/// not.
#[tokio::test]
async fn sync_removes_what_the_tree_no_longer_has() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;
    std::fs::remove_file(dir.path().join("docs/mounts.md")).unwrap();

    // A second ingest leaves the document behind, which is why `sync` exists.
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;
    assert_eq!(
        hits(&ok(dir.path(), &["search", "/notes.db", "kernel"]).await),
        ["docs/mounts.md"],
        "ingest only ever adds"
    );

    assert_eq!(
        ok(dir.path(), &["sync", "/notes.db", "/docs"]).await,
        "synced 0 file(s), 2 unchanged, removed 1 document(s)\n"
    );
    assert_eq!(
        ok(dir.path(), &["search", "/notes.db", "kernel"]).await,
        "no matches\n"
    );
}

/// Only files whose stamp moved are read again — and `--force` is the way to say otherwise.
#[tokio::test]
async fn sync_rereads_only_what_changed_unless_forced() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;

    assert_eq!(
        ok(dir.path(), &["sync", "/notes.db", "/docs"]).await,
        "synced 0 file(s), 3 unchanged, removed 0 document(s)\n"
    );
    assert_eq!(
        ok(dir.path(), &["sync", "/notes.db", "/docs", "--force"]).await,
        "synced 3 file(s), 0 unchanged, removed 0 document(s)\n"
    );

    // A file whose length moved is read again without being forced, and the store holds the new
    // words rather than the old.
    std::fs::write(
        dir.path().join("docs/mounts.md"),
        "A mount is a path the kernel answers on, and unmounting is a syscall.\n",
    )
    .unwrap();
    assert_eq!(
        ok(dir.path(), &["sync", "/notes.db", "/docs"]).await,
        "synced 1 file(s), 2 unchanged, removed 0 document(s)\n"
    );
    assert_eq!(
        hits(&ok(dir.path(), &["search", "/notes.db", "unmounting"]).await),
        ["docs/mounts.md"]
    );
}

/// A directory that is gone entirely means an empty tree under it.
#[tokio::test]
async fn sync_of_a_path_that_is_gone_empties_it() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;
    std::fs::remove_dir_all(dir.path().join("docs")).unwrap();

    assert_eq!(
        ok(dir.path(), &["sync", "/notes.db", "/docs"]).await,
        "synced 0 file(s), 0 unchanged, removed 3 document(s)\n"
    );
    assert_eq!(ok(dir.path(), &["list", "/"]).await, "notes.db\t0 document(s)\n");
}

/// `sync` is scoped to the paths it was given, so it does not touch what another `ingest` put
/// in from somewhere else.
#[tokio::test]
async fn sync_is_scoped_to_the_paths_it_was_given() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs", "/docs-other"]).await;
    std::fs::remove_dir_all(dir.path().join("docs")).unwrap();

    assert_eq!(
        ok(dir.path(), &["sync", "/notes.db", "/docs"]).await,
        "synced 0 file(s), 0 unchanged, removed 3 document(s)\n"
    );
    assert_eq!(
        hits(&ok(dir.path(), &["search", "/notes.db", "borrow"]).await),
        ["docs-other/x.md"],
        "what came from `/docs-other` was never in scope"
    );
}

/// `list` answers about one directory: the stores in it, and nothing else that is in it.
#[tokio::test]
async fn list_counts_stores_and_passes_over_everything_else() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;
    ok(dir.path(), &["init", "/second.db"]).await;
    // A plain file and somebody else's SQLite database, both beside the stores.
    std::fs::write(dir.path().join("plain.txt"), "not a store\n").unwrap();
    std::fs::write(dir.path().join("foreign.db"), b"SQLite format 3\0not really").unwrap();

    assert_eq!(
        ok(dir.path(), &["list", "/"]).await,
        "notes.db\t3 document(s)\nsecond.db\t0 document(s)\n",
        "sorted by name, and only the stores"
    );

    // A directory with no store in it says so rather than answering with nothing.
    assert_eq!(
        ok(dir.path(), &["list", "/docs"]).await,
        "no stores here — `docstore init <STORE>` makes one\n"
    );
}

/// Dropping forgets the store and leaves the files it was built from alone.
#[tokio::test]
async fn drop_removes_the_store_and_not_the_corpus() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;

    assert_eq!(
        ok(dir.path(), &["drop", "/notes.db"]).await,
        "dropped /notes.db\n"
    );
    assert!(!dir.path().join("notes.db").exists());
    assert!(
        dir.path().join("docs/ownership.md").is_file(),
        "the corpus is untouched"
    );
}

/// The whole of what `drop` does that `rm` does not: a store is a path, so one mistyped argument
/// would otherwise be somebody's data.
#[tokio::test]
async fn drop_refuses_what_is_not_a_store() {
    let dir = ready().await;
    std::fs::write(dir.path().join("plain.txt"), "not a store\n").unwrap();

    for arg in ["/plain.txt", "/docs/ownership.md"] {
        let out = run(dir.path(), &["drop", arg]).await;
        assert_eq!(out.exit_code, 1, "{arg}");
        assert!(
            dir.path()
                .join(arg.trim_start_matches('/'))
                .try_exists()
                .unwrap(),
            "{arg} was removed"
        );
    }

    let missing = run(dir.path(), &["drop", "/nope.db"]).await;
    assert_eq!(missing.exit_code, 1);
}

/// The tables are shared with `memstore`, so nothing about a file's *structure* says which
/// command wrote it. `meta.kind` does, and these are the three places it has to be asked.
///
/// Without it `list` would count somebody's memories as documents, `drop` would delete them, and
/// `search` would answer from rows whose path is null.
#[tokio::test]
async fn a_memstore_file_is_not_a_docstore_one() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs"]).await;

    // A store of the other kind, beside this one, made through the shared schema.
    let theirs = dir.path().join("memories.db");
    drop(
        cortex_exec_storebase::sqlite::create(&theirs, "memstore").expect("a memstore store"),
    );

    assert_eq!(
        ok(dir.path(), &["list", "/"]).await,
        "notes.db\t3 document(s)\n",
        "only this kind is listed"
    );

    for args in [
        vec!["search", "/memories.db", "ownership"],
        vec!["drop", "/memories.db"],
        vec!["ingest", "/memories.db", "/docs"],
    ] {
        let out = run(dir.path(), &args).await;
        assert_eq!(out.exit_code, 1, "{args:?}");
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(
            said.contains("memstore") || said.contains("not a store"),
            "{args:?} said: {said}"
        );
    }
    assert!(theirs.is_file(), "and it is still there");
}

/// `--help` on the name, on stdout, succeeding — a caller asked a question and got an answer.
#[tokio::test]
async fn help_is_an_answer_and_a_usage_error_is_not() {
    let dir = tree();

    let helped = run(dir.path(), &["--help"]).await;
    assert_eq!(helped.exit_code, 0);
    assert!(helped.stderr.is_empty());
    let said = String::from_utf8_lossy(&helped.stdout);
    for command in ["init", "ingest", "search", "sync", "purge", "list", "drop"] {
        assert!(said.contains(command), "{command} is missing from: {said}");
    }

    // The half a runtime that intercepted `--help` could not have covered.
    let sub = run(dir.path(), &["sync", "--help"]).await;
    assert_eq!(sub.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&sub.stdout).contains("--force"),
        "a subcommand answers its own help"
    );

    // Getting the usage wrong is `2` and goes to stderr, which is the distinction a caller acts
    // on: this line can be reissued differently, where a `1` was understood and did not work.
    let wrong = run(dir.path(), &["ingest"]).await;
    assert_eq!(wrong.exit_code, 2);
    assert!(wrong.stdout.is_empty());
    assert!(!wrong.stderr.is_empty());
}

/// The usage line is spelled with the name it was invoked by, so an executable registered under
/// another name does not tell a caller to run a command they do not have. This is what clap's
/// `string` feature is on for.
#[tokio::test]
async fn usage_names_the_name_it_was_called_by() {
    let execs = ExecutableSet::new().register("kb", "the knowledge base", DocStore::new());
    let out = execs
        .invoke(
            &ExecCall {
                name: "kb".into(),
                args: vec!["--help".into()],
                env: Default::default(),
                cwd: None,
            },
            None,
        )
        .await
        .expect("kb is registered");

    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("Usage: kb"), "{said}");
    assert!(!said.contains("Usage: docstore"), "{said}");
}

/// No colour: there is no tty here and this output is bytes on a wire.
#[tokio::test]
async fn nothing_it_writes_carries_an_escape() {
    let dir = ready().await;
    for args in [
        vec!["--help"],
        vec!["search", "--help"],
        vec!["purge"],
        vec!["search", "/notes.db", "ownership"],
        vec!["list", "/"],
    ] {
        let out = run(dir.path(), &args).await;
        let written = [out.stdout, out.stderr].concat();
        assert!(
            !written.contains(&0x1b),
            "{args:?} wrote an escape: {:?}",
            String::from_utf8_lossy(&written)
        );
    }
}

/// A store is a path in the tree, so there is nothing to reach with nothing mounted. That holds
/// even for `search` and `purge`, which open no file of the corpus: what they need the mount for
/// is the store itself.
#[tokio::test]
async fn every_command_needs_the_mount() {
    for args in [
        vec!["init", "/notes.db"],
        vec!["ingest", "/notes.db", "/docs"],
        vec!["search", "/notes.db", "ownership"],
        vec!["sync", "/notes.db", "/docs"],
        vec!["purge", "/notes.db", "/docs"],
        vec!["list", "/"],
        vec!["drop", "/notes.db"],
    ] {
        let out = registered()
            .invoke(&call(&args), None)
            .await
            .expect("docstore is registered");
        assert_eq!(out.exit_code, 1, "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("nothing is mounted"),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Today's limit, written down as a test so it fails when `cwd` starts arriving: a relative
/// argument needs a directory to resolve against, and a substituted root would read a different
/// file and say nothing about it.
#[tokio::test]
async fn a_relative_path_is_refused_while_the_backend_reports_no_directory() {
    let dir = ready().await;

    let out = run(dir.path(), &["ingest", "/notes.db", "docs"]).await;
    assert_eq!(out.exit_code, 1);
    assert!(out.stdout.is_empty());

    // `list` with no argument is the same case: its default is the caller's own directory.
    let listed = run(dir.path(), &["list"]).await;
    assert_eq!(listed.exit_code, 1);
}

/// The bound reaches the store rather than trimming an answer that was already built.
#[tokio::test]
async fn a_search_is_bounded_and_the_count_is_the_callers_to_raise() {
    let dir = ready().await;
    ok(dir.path(), &["ingest", "/notes.db", "/docs", "/docs-other"]).await;

    let all = ok(dir.path(), &["search", "/notes.db", "the"]).await;
    assert!(hits(&all).len() > 1, "{all}");

    let one = ok(dir.path(), &["search", "/notes.db", "-n", "1", "the"]).await;
    assert_eq!(hits(&one).len(), 1, "{one}");
}
