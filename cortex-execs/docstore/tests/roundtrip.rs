//! Ingest a tree, then find it again — all through the `docstore` binary itself.
//!
//! The program under test is the one cargo just built, run as a program: a working directory, a
//! command line, and what came back on each stream. That is not an accident of convenience. A
//! path argument here means what it means to the shell that typed it, and a working directory is
//! a property of a process — so a test that called into this code in-process would have to either
//! spell every path absolutely, which is not how anybody runs it, or share one directory between
//! every test in the binary.
//!
//! What these pin is the observable contract: what a walk picks up, what a second `ingest` means,
//! what `sync` removes, which stream each answer goes to, and what exit code rides with it.

use std::path::Path;
use std::process::Output;

/// One line, run in `tree` the way a shell standing there would run it.
fn docstore(tree: &Path, args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_docstore"))
        .current_dir(tree)
        .args(args)
        .output()
        .expect("the docstore binary runs")
}

/// Run one line and insist it succeeded, handing back stdout.
fn ok(tree: &Path, args: &[&str]) -> String {
    let out = docstore(tree, args);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
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
    at(
        "docs/mounts.md",
        "A mount is a path the kernel answers on.\n",
    );
    at(
        "docs/sub/nested.md",
        "Nested prose about lifetimes and elision.\n",
    );
    // Not in the allowlist, so a walk should leave it alone.
    std::fs::write(dir.path().join("docs/ignore.bin"), [0xff, 0x00]).unwrap();
    // Dotted, so the walk should not descend into it.
    at("docs/.hidden/secret.md", "Nobody should index this.\n");
    // A sibling whose name starts with the same letters as `docs`.
    at(
        "docs-other/x.md",
        "The borrow checker elsewhere entirely.\n",
    );
    dir
}

/// A tree with a store in it, already made.
fn ready() -> tempfile::TempDir {
    let dir = tree();
    ok(dir.path(), &["init", "notes.db"]);
    dir
}

/// The paths a search answered with, in the order it answered them.
fn hits(said: &str) -> Vec<String> {
    said.lines()
        .filter(|line| !line.starts_with('\t'))
        .filter_map(|line| line.split('\t').nth(1).map(str::to_owned))
        .collect()
}

#[test]
fn ingest_then_search_finds_the_file_by_its_words() {
    let dir = ready();
    assert_eq!(
        ok(dir.path(), &["ingest", "notes.db", "docs"]),
        "indexed 3 file(s)\n",
        "the allowlist took the three .md files and nothing else"
    );

    let said = ok(dir.path(), &["search", "notes.db", "ownership"]);
    assert_eq!(hits(&said), ["docs/ownership.md"]);
    // The snippet is the line under the hit, cut around what was asked for.
    assert!(
        said.lines().nth(1).is_some_and(|l| l.contains("ownership")),
        "{said}"
    );
}

/// A document is filed under the path it was given, so where the command was run from is what a
/// relative argument means — and `list` with nothing after it asks about that same directory.
#[test]
fn a_path_means_what_it_meant_to_the_shell_that_typed_it() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "./docs/sub"]);

    assert_eq!(
        hits(&ok(dir.path(), &["search", "notes.db", "lifetimes"])),
        ["docs/sub/nested.md"],
        "`.` names nothing, so it is not part of what the document is called"
    );
    assert_eq!(ok(dir.path(), &["list"]), "notes.db\t1 document(s)\n");

    // The same file named absolutely is a different document, because it is a different name —
    // and this program does not decide on the caller's behalf that the two are one file.
    let absolute = dir.path().join("docs/sub").to_str().unwrap().to_owned();
    ok(dir.path(), &["ingest", "notes.db", &absolute]);
    let both = hits(&ok(dir.path(), &["search", "notes.db", "lifetimes"]));
    assert_eq!(both.len(), 2, "{both:?}");
    assert!(
        both.iter().any(|p| p.starts_with('/')),
        "an absolute argument is filed absolutely: {both:?}"
    );
}

/// The property that lets an agent run `ingest` without keeping track of what it has run.
#[test]
fn ingesting_twice_means_the_same_as_ingesting_once() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);
    ok(dir.path(), &["ingest", "notes.db", "docs"]);

    assert_eq!(ok(dir.path(), &["list", "."]), "notes.db\t3 document(s)\n");
    let said = ok(dir.path(), &["search", "notes.db", "ownership"]);
    assert_eq!(hits(&said), ["docs/ownership.md"], "one document, not two");
}

/// Overlapping arguments walk the same file twice; the count must not say so.
#[test]
fn overlapping_arguments_are_counted_once() {
    let dir = ready();
    assert_eq!(
        ok(dir.path(), &["ingest", "notes.db", "docs", "docs/sub"]),
        "indexed 3 file(s)\n"
    );
}

/// The walk's rules: an unlisted extension and a dotted directory are not in the store.
#[test]
fn the_walk_skips_dotfiles_and_unlisted_extensions() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);

    let nothing = ok(dir.path(), &["search", "notes.db", "Nobody"]);
    assert_eq!(
        nothing, "no matches\n",
        "the dotted directory was not walked"
    );
    assert_eq!(ok(dir.path(), &["list", "."]), "notes.db\t3 document(s)\n");
}

/// The bytes were read when they were ingested, so a search does not open the corpus again.
#[test]
fn search_reads_no_file_of_the_corpus() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);
    std::fs::remove_dir_all(dir.path().join("docs")).unwrap();

    let said = ok(dir.path(), &["search", "notes.db", "ownership"]);
    assert_eq!(hits(&said), ["docs/ownership.md"]);
}

/// Nothing near the question is no hits and a zero: the store was read and holds nothing near
/// this, which is an answer and not a failure.
#[test]
fn a_question_with_no_answer_is_still_an_answer() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);

    for query in ["zygote", "!!!"] {
        let out = docstore(dir.path(), &["search", "notes.db", query]);
        assert_eq!(out.status.code(), Some(0), "{query:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "no matches\n");
    }
}

/// FTS5 reads its pattern as an expression, and a caller was never told that language.
#[test]
fn a_query_that_looks_like_an_expression_is_read_as_words() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);

    for query in [
        "NOT",
        "ownership AND",
        "\"unbalanced",
        "a:b",
        "*",
        "(",
        "^x",
        "a OR",
    ] {
        let out = docstore(dir.path(), &["search", "notes.db", query]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{query:?} was read as syntax: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // A word starting with `-` is the parser's business and not FTS5's: clap takes it for an
    // option, and `--` is how a caller says it is a word. That is the ordinary command-line
    // convention, and borrowing it is the reason there is a parser here at all.
    let flagged = docstore(dir.path(), &["search", "notes.db", "-x"]);
    assert_eq!(
        flagged.status.code(),
        Some(2),
        "an unknown option is a misunderstood line"
    );
    let quoted = docstore(dir.path(), &["search", "notes.db", "--", "-x"]);
    assert_eq!(
        quoted.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&quoted.stderr)
    );
}

/// What indexing the body rather than a list of terms buys: `porter` stems, so a question asked
/// in one form finds a file written in another.
#[test]
fn a_file_is_found_by_a_word_in_another_form() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);

    for asked in ["mount", "mounts", "mounting", "answer", "answers"] {
        assert_eq!(
            hits(&ok(dir.path(), &["search", "notes.db", asked])),
            ["docs/mounts.md"],
            "{asked} did not reach the file"
        );
    }
}

/// The snippet is cut from the body as it was written — the store holds the original, which is
/// what lets the index be rebuilt under another tokenizer later without a re-ingest.
#[test]
fn the_snippet_is_the_body_as_it_was_written() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);

    let said = ok(dir.path(), &["search", "notes.db", "kernel"]);
    let snippet = said.lines().nth(1).expect("a snippet under the hit");
    assert!(
        snippet.contains("A mount is a path the kernel answers on"),
        "case and wording survive: {snippet}"
    );
}

/// A store that was never made is an error, not an empty index answering "no matches" — which
/// reads as a corpus with nothing in it.
#[test]
fn a_store_that_is_not_there_is_not_made_by_using_it() {
    let dir = tree();
    for args in [
        vec!["search", "nope.db", "ownership"],
        vec!["ingest", "nope.db", "docs"],
        vec!["purge", "nope.db", "docs"],
    ] {
        let out = docstore(dir.path(), &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?} wrote something to read");
        assert!(
            !dir.path().join("nope.db").exists(),
            "{args:?} made a store"
        );
    }
}

#[test]
fn init_will_not_make_a_store_twice() {
    let dir = ready();
    let again = docstore(dir.path(), &["init", "notes.db"]);
    assert_eq!(again.status.code(), Some(1));
    assert!(again.stdout.is_empty(), "nothing to read as a store's path");
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("notes.db"),
        "the store is named as the caller named it"
    );
}

/// `purge` is `ingest` undone, and a sibling whose name merely starts the same way is not under
/// the path given.
#[test]
fn purge_takes_a_subtree_out_and_leaves_a_sibling() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs", "docs-other"]);
    assert_eq!(ok(dir.path(), &["list", "."]), "notes.db\t4 document(s)\n");

    assert_eq!(
        ok(dir.path(), &["purge", "notes.db", "docs"]),
        "purged 3 document(s)\n"
    );
    let said = ok(dir.path(), &["search", "notes.db", "borrow"]);
    assert_eq!(
        hits(&said),
        ["docs-other/x.md"],
        "`docs-other` is not under `docs`"
    );
}

/// The case `purge` exists for: the file is gone from the tree, and the document is not.
#[test]
fn purge_opens_no_file_of_the_corpus() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);
    std::fs::remove_dir_all(dir.path().join("docs")).unwrap();

    assert_eq!(
        ok(dir.path(), &["purge", "notes.db", "docs"]),
        "purged 3 document(s)\n"
    );
}

/// The gap a second `ingest` leaves: added and changed files it handles, a removed one it does
/// not.
#[test]
fn sync_removes_what_the_tree_no_longer_has() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);
    std::fs::remove_file(dir.path().join("docs/mounts.md")).unwrap();

    // A second ingest leaves the document behind, which is why `sync` exists.
    ok(dir.path(), &["ingest", "notes.db", "docs"]);
    assert_eq!(
        hits(&ok(dir.path(), &["search", "notes.db", "kernel"])),
        ["docs/mounts.md"],
        "ingest only ever adds"
    );

    assert_eq!(
        ok(dir.path(), &["sync", "notes.db", "docs"]),
        "synced 0 file(s), 2 unchanged, removed 1 document(s)\n"
    );
    assert_eq!(
        ok(dir.path(), &["search", "notes.db", "kernel"]),
        "no matches\n"
    );
}

/// Only files whose stamp moved are read again — and `--force` is the way to say otherwise.
#[test]
fn sync_rereads_only_what_changed_unless_forced() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);

    assert_eq!(
        ok(dir.path(), &["sync", "notes.db", "docs"]),
        "synced 0 file(s), 3 unchanged, removed 0 document(s)\n"
    );
    assert_eq!(
        ok(dir.path(), &["sync", "notes.db", "docs", "--force"]),
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
        ok(dir.path(), &["sync", "notes.db", "docs"]),
        "synced 1 file(s), 2 unchanged, removed 0 document(s)\n"
    );
    assert_eq!(
        hits(&ok(dir.path(), &["search", "notes.db", "unmounting"])),
        ["docs/mounts.md"]
    );
}

/// A directory that is gone entirely means an empty tree under it.
#[test]
fn sync_of_a_path_that_is_gone_empties_it() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);
    std::fs::remove_dir_all(dir.path().join("docs")).unwrap();

    assert_eq!(
        ok(dir.path(), &["sync", "notes.db", "docs"]),
        "synced 0 file(s), 0 unchanged, removed 3 document(s)\n"
    );
    assert_eq!(ok(dir.path(), &["list", "."]), "notes.db\t0 document(s)\n");
}

/// `sync` is scoped to the paths it was given, so it does not touch what another `ingest` put in
/// from somewhere else.
#[test]
fn sync_is_scoped_to_the_paths_it_was_given() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs", "docs-other"]);
    std::fs::remove_dir_all(dir.path().join("docs")).unwrap();

    assert_eq!(
        ok(dir.path(), &["sync", "notes.db", "docs"]),
        "synced 0 file(s), 0 unchanged, removed 3 document(s)\n"
    );
    assert_eq!(
        hits(&ok(dir.path(), &["search", "notes.db", "borrow"])),
        ["docs-other/x.md"],
        "what came from `docs-other` was never in scope"
    );
}

/// `list` answers about one directory: the stores in it, and nothing else that is in it.
#[test]
fn list_counts_stores_and_passes_over_everything_else() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);
    ok(dir.path(), &["init", "second.db"]);
    // A plain file and somebody else's SQLite database, both beside the stores.
    std::fs::write(dir.path().join("plain.txt"), "not a store\n").unwrap();
    std::fs::write(
        dir.path().join("foreign.db"),
        b"SQLite format 3\0not really",
    )
    .unwrap();

    assert_eq!(
        ok(dir.path(), &["list", "."]),
        "notes.db\t3 document(s)\nsecond.db\t0 document(s)\n",
        "sorted by name, and only the stores"
    );

    // A directory with no store in it says so rather than answering with nothing.
    assert_eq!(
        ok(dir.path(), &["list", "docs"]),
        "no stores here — `docstore init <STORE>` makes one\n"
    );
}

/// Dropping forgets the store and leaves the files it was built from alone.
#[test]
fn drop_removes_the_store_and_not_the_corpus() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);

    assert_eq!(ok(dir.path(), &["drop", "notes.db"]), "dropped notes.db\n");
    assert!(!dir.path().join("notes.db").exists());
    assert!(
        dir.path().join("docs/ownership.md").is_file(),
        "the corpus is untouched"
    );
}

/// The whole of what `drop` does that `rm` does not: a store is a path, so one mistyped argument
/// would otherwise be somebody's data.
#[test]
fn drop_refuses_what_is_not_a_store() {
    let dir = ready();
    std::fs::write(dir.path().join("plain.txt"), "not a store\n").unwrap();

    for arg in ["plain.txt", "docs/ownership.md"] {
        let out = docstore(dir.path(), &["drop", arg]);
        assert_eq!(out.status.code(), Some(1), "{arg}");
        assert!(
            dir.path().join(arg).try_exists().unwrap(),
            "{arg} was removed"
        );
    }

    let missing = docstore(dir.path(), &["drop", "nope.db"]);
    assert_eq!(missing.status.code(), Some(1));
}

/// The tables are shared with `memstore`, so nothing about a file's *structure* says which
/// command wrote it. `meta.kind` does, and these are the three places it has to be asked.
///
/// Without it `list` would count somebody's memories as documents, `drop` would delete them, and
/// `search` would answer from rows whose path is null.
#[test]
fn a_memstore_file_is_not_a_docstore_one() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs"]);

    // A store of the other kind, beside this one, made through the shared schema.
    let theirs = dir.path().join("memories.db");
    drop(cortex_exec_storebase::sqlite::create(&theirs, "memstore").expect("a memstore store"));

    assert_eq!(
        ok(dir.path(), &["list", "."]),
        "notes.db\t3 document(s)\n",
        "only this kind is listed"
    );

    for args in [
        vec!["search", "memories.db", "ownership"],
        vec!["drop", "memories.db"],
        vec!["ingest", "memories.db", "docs"],
    ] {
        let out = docstore(dir.path(), &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(
            said.contains("memstore") || said.contains("not a store"),
            "{args:?} said: {said}"
        );
    }
    assert!(theirs.is_file(), "and it is still there");
}

/// `--help` on the name, on stdout, succeeding — a caller asked a question and got an answer.
#[test]
fn help_is_an_answer_and_a_usage_error_is_not() {
    let dir = tree();

    let helped = docstore(dir.path(), &["--help"]);
    assert_eq!(helped.status.code(), Some(0));
    assert!(helped.stderr.is_empty());
    let said = String::from_utf8_lossy(&helped.stdout);
    assert!(said.contains("Usage: docstore"), "{said}");
    for command in ["init", "ingest", "search", "sync", "purge", "list", "drop"] {
        assert!(said.contains(command), "{command} is missing from: {said}");
    }

    // The half a hand-written option loop always ends up missing.
    let sub = docstore(dir.path(), &["sync", "--help"]);
    assert_eq!(sub.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&sub.stdout).contains("--force"),
        "a subcommand answers its own help"
    );

    // Getting the usage wrong is `2` and goes to stderr, which is the distinction a caller acts
    // on: this line can be reissued differently, where a `1` was understood and did not work.
    let wrong = docstore(dir.path(), &["ingest"]);
    assert_eq!(wrong.status.code(), Some(2));
    assert!(wrong.stdout.is_empty());
    assert!(!wrong.stderr.is_empty());
}

/// No colour: this output went to a pipe, and an escape chosen for a terminal would be bytes in
/// whatever the caller does with it.
#[test]
fn nothing_it_writes_carries_an_escape() {
    let dir = ready();
    for args in [
        vec!["--help"],
        vec!["search", "--help"],
        vec!["purge"],
        vec!["search", "notes.db", "ownership"],
        vec!["list", "."],
    ] {
        let out = docstore(dir.path(), &args);
        let written = [out.stdout, out.stderr].concat();
        assert!(
            !written.contains(&0x1b),
            "{args:?} wrote an escape: {:?}",
            String::from_utf8_lossy(&written)
        );
    }
}

/// The bound reaches the store rather than trimming an answer that was already built.
#[test]
fn a_search_is_bounded_and_the_count_is_the_callers_to_raise() {
    let dir = ready();
    ok(dir.path(), &["ingest", "notes.db", "docs", "docs-other"]);

    let all = ok(dir.path(), &["search", "notes.db", "the"]);
    assert!(hits(&all).len() > 1, "{all}");

    let one = ok(dir.path(), &["search", "notes.db", "-n", "1", "the"]);
    assert_eq!(hits(&one).len(), 1, "{one}");
}
