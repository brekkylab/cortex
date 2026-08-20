//! Ingest a tree through a mount, then find it again — all through the one `index` name.
//!
//! The mount here is a plain directory, which is what a [`Mount`] is from this executable's
//! side: a path any process can `open`. Standing one up for real needs a binding, a libfuse
//! provider and a kernel, and none of that is what these test.

use std::path::{Path, PathBuf};

use cortex::exec::{ExecCall, ExecutableSet};
use cortex::fs::Mount;
use cortex_exec_index::Index;

struct Mounted(PathBuf);

impl Mount for Mounted {
    fn mountpoint(&self) -> &Path {
        &self.0
    }
}

/// The set as a consumer registers it: one name, and a root the stores live under.
///
/// The root is a directory of this test's own — outside anything mounted, which is the
/// arrangement the executable checks for.
fn registered(root: &Path) -> std::io::Result<ExecutableSet> {
    Ok(ExecutableSet::new().register("index", Index::SUMMARY, Index::new(root)?))
}

/// A tree to index, and a store root beside it that is *not* under it.
fn fixture() -> (tempfile::TempDir, tempfile::TempDir) {
    (tree(), tempfile::tempdir().expect("a store root"))
}

fn call(args: &[&str]) -> ExecCall {
    ExecCall {
        name: "index".into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: Default::default(),
        // `None` — a backend that reports no working directory, so every path below is
        // workspace-absolute and the test is written the way a caller has to write it then.
        cwd: None,
    }
}

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir(dir.path().join("notes")).unwrap();
    std::fs::write(
        dir.path().join("notes/ownership.md"),
        "Rust ownership is what makes the borrow checker possible.\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("notes/mounts.md"),
        "A mount is a path the kernel answers on.\n",
    )
    .unwrap();
    // Not in the allowlist, so a walk should leave it alone.
    std::fs::write(dir.path().join("notes/ignore.bin"), [0xff, 0x00]).unwrap();
    dir
}

#[tokio::test]
async fn a_directory_is_indexed_and_then_found() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    let out = execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .expect("index is registered");
    assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "indexed 2 file(s)\n",
        "the .bin is not one of them"
    );

    let out = execs
        .invoke(
            &call(&["search", "notes", "borrow", "checker"]),
            Some(&mount),
        )
        .await
        .unwrap();
    let found = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        found.contains("notes/ownership.md"),
        "the hit names the workspace path, not the host one: {found}"
    );
    assert!(!found.contains("notes/mounts.md"), "{found}");
}

/// The property that lets an agent run `index ingest` without keeping track of what it
/// already ingested.
#[tokio::test]
async fn ingesting_twice_leaves_one_document() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    for _ in 0..2 {
        let out = execs
            .invoke(
                &call(&["ingest", "notes", "/notes/ownership.md"]),
                Some(&mount),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
    }

    let out = execs
        .invoke(&call(&["search", "notes", "ownership"]), Some(&mount))
        .await
        .unwrap();
    let found = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        found.matches("notes/ownership.md").count(),
        1,
        "the second ingest replaced the first: {found}"
    );
}

#[tokio::test]
async fn the_limit_is_taken_from_n() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();

    let out = execs
        .invoke(&call(&["search", "notes", "-n", "1", "a"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    // Two lines per hit — the header and its snippet.
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 2);
}

/// A name that needs a file says it cannot reach one, rather than reading something else.
#[tokio::test]
async fn ingest_without_a_mount_refuses() {
    let root = tempfile::tempdir().unwrap();
    let execs = registered(root.path()).unwrap();

    let out = execs
        .invoke(&call(&["ingest", "notes", "/notes"]), None)
        .await
        .unwrap();
    assert_eq!(out.exit_code, 1);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nothing is mounted"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Searching needs no mount, because the corpus was read when it was ingested — which is why
/// the pair can share one name that takes `Option<&dyn Mount>`.
#[tokio::test]
async fn search_answers_without_a_mount() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();

    let out = execs
        .invoke(&call(&["search", "notes", "kernel"]), None)
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(String::from_utf8_lossy(&out.stdout).contains("notes/mounts.md"));
}

/// The line under a hit is cut around what was queried, not off the top of the file — and
/// the match sits in the *middle* of the cut.
///
/// The filler is 1600 characters on each side, so a head-of-body snippet could not reach the
/// match however it was trimmed. Asserting the whole sentence and both ellipses is what
/// separates a centred excerpt from tantivy's own window, whose edge can land on the term and
/// take the rest of its sentence with it.
#[tokio::test]
async fn a_snippet_is_cut_around_the_query() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    let filler = "a paragraph about nothing in particular. ".repeat(40);
    let body = format!("{filler}\nlifetimes are what the borrow checker counts.\n{filler}");
    std::fs::write(tree.path().join("notes/deep.md"), &body).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    let out = execs
        .invoke(&call(&["search", "notes", "lifetimes"]), None)
        .await
        .unwrap();

    let said = String::from_utf8_lossy(&out.stdout);
    let snippet = said
        .lines()
        .skip_while(|line| !line.contains("notes/deep.md"))
        .nth(1)
        .unwrap_or_else(|| panic!("a snippet under the hit: {said}"));
    assert!(
        snippet.contains("lifetimes are what the borrow checker counts"),
        "the match itself is in it: {snippet:?}"
    );
    assert!(
        snippet.starts_with("\t…") && snippet.ends_with('…'),
        "with the file going on above and below it: {snippet:?}"
    );
}

/// A hit whose *name* matched has no body term to cut around, and shows the opening instead.
#[tokio::test]
async fn a_name_only_match_shows_the_head_of_the_body() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    std::fs::write(
        tree.path().join("notes/lifetimes.md"),
        "Every value has exactly one owner.\n",
    )
    .unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    let out = execs
        .invoke(&call(&["search", "notes", "lifetimes"]), None)
        .await
        .unwrap();

    let said = String::from_utf8_lossy(&out.stdout);
    let snippet = said
        .lines()
        .skip_while(|line| !line.contains("notes/lifetimes.md"))
        .nth(1)
        .unwrap_or_else(|| panic!("a snippet under the hit: {said}"));
    assert_eq!(
        snippet, "\tEvery value has exactly one owner.",
        "the whole body, with no ellipsis: nothing was cut"
    );
}

/// Today's limit, written down as a test so it fails when `cwd` starts arriving.
#[tokio::test]
async fn a_relative_path_is_refused_while_there_is_no_cwd() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    let out = execs
        .invoke(&call(&["ingest", "corpus", "notes"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 1, "{}", String::from_utf8_lossy(&out.stdout));
}

/// `--help` on the name, on stdout, succeeding — a caller asked a question and got an answer.
#[tokio::test]
async fn help_is_an_answer_and_not_a_failure() {
    let root = tempfile::tempdir().unwrap();
    let execs = registered(root.path()).unwrap();

    let out = execs.invoke(&call(&["--help"]), None).await.unwrap();
    assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("ingest") && said.contains("search"), "{said}");
    assert!(out.stderr.is_empty());
}

/// The half a runtime that intercepted `--help` could not have covered: each subcommand
/// answers for itself, with its own options.
#[tokio::test]
async fn each_subcommand_answers_its_own_help() {
    let root = tempfile::tempdir().unwrap();
    let execs = registered(root.path()).unwrap();

    let out = execs
        .invoke(&call(&["search", "--help"]), None)
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("--limit"), "the option it takes: {said}");
    assert!(said.contains("index search"), "usage names both: {said}");

    let out = execs
        .invoke(&call(&["ingest", "--help"]), None)
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("PATH"), "{said}");
    assert!(!said.contains("--limit"), "not the other one's: {said}");
}

/// Getting the usage wrong is `2` and goes to stderr, which is the distinction
/// `clap::Error::exit_code` already draws for us.
#[tokio::test]
async fn a_bare_name_and_an_unknown_subcommand_are_usage_errors() {
    let root = tempfile::tempdir().unwrap();
    let execs = registered(root.path()).unwrap();

    let bare = execs.invoke(&call(&[]), None).await.unwrap();
    assert_eq!(bare.exit_code, 2);
    let said = String::from_utf8_lossy(&bare.stderr);
    assert!(said.contains("ingest") && said.contains("search"), "{said}");

    let wrong = execs.invoke(&call(&["purge"]), None).await.unwrap();
    assert_eq!(wrong.exit_code, 2);
    assert!(
        String::from_utf8_lossy(&wrong.stderr).contains("purge"),
        "{:?}",
        String::from_utf8_lossy(&wrong.stderr)
    );

    let bad = execs
        .invoke(&call(&["search", "notes", "-n", "many", "x"]), None)
        .await
        .unwrap();
    assert_eq!(bad.exit_code, 2, "not a count");
}

/// The usage line is spelled with the name it was invoked by, so an executable registered
/// under another name does not tell a caller to run a command they do not have. This is what
/// clap's `string` feature is on for — without it the name has to be `&'static str`.
#[tokio::test]
async fn usage_names_the_name_it_was_called_by() {
    let root = tempfile::tempdir().unwrap();
    let execs =
        ExecutableSet::new().register("kb", "the knowledge base", Index::new(root.path()).unwrap());

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
        .unwrap();
    // The usage line specifically: the prose around it says "indexed" for honest reasons, so
    // a bare `contains("index")` would fail on its own documentation.
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("Usage: kb"), "{said}");
    assert!(!said.contains("Usage: index"), "{said}");
}

/// No colour: there is no tty here and this output is bytes on a wire.
#[tokio::test]
async fn nothing_it_writes_carries_an_escape() {
    let root = tempfile::tempdir().unwrap();
    let execs = registered(root.path()).unwrap();

    for args in [vec!["--help"], vec!["search", "--help"], vec!["purge"]] {
        let out = execs.invoke(&call(&args), None).await.unwrap();
        let written = [out.stdout, out.stderr].concat();
        assert!(
            !written.contains(&0x1b),
            "{args:?} wrote an escape: {:?}",
            String::from_utf8_lossy(&written)
        );
    }
}

/// The check the whole arrangement rests on: a store is *which* index, never *where*.
#[tokio::test]
async fn a_store_name_that_is_a_path_is_refused() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    for name in ["../escape", "/absolute", "sub/dir", ".hidden"] {
        let out = execs
            .invoke(&call(&["ingest", name, "/notes"]), Some(&mount))
            .await
            .unwrap();
        assert_ne!(out.exit_code, 0, "{name} was accepted as a store");
        assert!(
            !root.path().join("escape").exists() && !Path::new("/absolute").exists(),
            "{name} reached outside the root"
        );
    }
}

/// `ingest` makes the store it names, and `list` is how a caller sees what there is —
/// including the one a typo made.
#[tokio::test]
async fn ingest_creates_the_store_and_list_reports_it() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    let empty = execs.invoke(&call(&["list"]), None).await.unwrap();
    assert!(
        String::from_utf8_lossy(&empty.stdout).contains("no stores yet"),
        "{:?}",
        String::from_utf8_lossy(&empty.stdout)
    );

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    execs
        .invoke(&call(&["ingest", "note", "/notes/mounts.md"]), Some(&mount))
        .await
        .unwrap();

    let listed = execs.invoke(&call(&["list"]), None).await.unwrap();
    let said = String::from_utf8_lossy(&listed.stdout);
    assert!(said.contains("notes	2 document(s)"), "{said}");
    assert!(
        said.contains("note	1 document(s)"),
        "the typo is visible: {said}"
    );
}

/// A store that was never made is `NotFound`, not an empty index answering "no matches" —
/// which would read as a corpus with nothing in it.
#[tokio::test]
async fn searching_a_store_that_does_not_exist_says_so() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();

    let out = execs
        .invoke(&call(&["search", "nope", "anything"]), None)
        .await
        .unwrap();
    assert_ne!(out.exit_code, 0);
    // Either clap refused it against the stores that exist, or the lookup did.
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("nope"), "{said}");
}

/// Dropping forgets the index and leaves the files it was built from alone.
#[tokio::test]
async fn drop_removes_the_store_and_not_the_corpus() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    let out = execs
        .invoke(&call(&["drop", "notes"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));

    assert!(!root.path().join("notes").exists(), "the store is gone");
    assert!(
        tree.path().join("notes/ownership.md").exists(),
        "the corpus is not"
    );
}

/// `purge` is `ingest` undone: the documents go, the store stays, and a later `ingest` of the
/// same path puts them back.
#[tokio::test]
async fn purge_takes_documents_back_out() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();

    let out = execs
        .invoke(&call(&["purge", "notes", "/notes/mounts.md"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "purged 1 document(s)\n"
    );

    let listed = execs.invoke(&call(&["list"]), None).await.unwrap();
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("notes\t1 document(s)"),
        "the store is still there with the other one in it: {:?}",
        String::from_utf8_lossy(&listed.stdout)
    );

    let found = execs
        .invoke(&call(&["search", "notes", "kernel"]), None)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&found.stdout).contains("no matches"),
        "{:?}",
        String::from_utf8_lossy(&found.stdout)
    );
}

/// A directory purges everything under it, so `purge` undoes the `ingest` that named the same
/// argument — whichever kind of thing that argument was.
#[tokio::test]
async fn purging_a_directory_takes_everything_under_it() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    let out = execs
        .invoke(&call(&["purge", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "purged 2 document(s)\n"
    );
}

/// The case `purge` exists for: the file is gone from the tree, and nothing is mounted either.
#[tokio::test]
async fn purge_needs_no_mount_and_no_file() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    std::fs::remove_file(tree.path().join("notes/ownership.md")).unwrap();

    let out = execs
        .invoke(&call(&["purge", "notes", "/notes/ownership.md"]), None)
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "purged 1 document(s)\n"
    );
}

/// A sibling whose name starts with the same letters is not under it.
#[tokio::test]
async fn purge_matches_whole_components() {
    let (tree, root) = fixture();
    std::fs::create_dir(tree.path().join("notes-other")).unwrap();
    std::fs::write(tree.path().join("notes-other/x.md"), "elsewhere\n").unwrap();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(
            &call(&["ingest", "notes", "/notes", "/notes-other"]),
            Some(&mount),
        )
        .await
        .unwrap();
    let out = execs
        .invoke(&call(&["purge", "notes", "/notes"]), None)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "purged 2 document(s)\n",
        "notes-other/x.md is not under notes"
    );

    let listed = execs.invoke(&call(&["list"]), None).await.unwrap();
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("notes\t1 document(s)"),
        "{:?}",
        String::from_utf8_lossy(&listed.stdout)
    );
}

/// The gap a second `ingest` leaves: added and changed files it handles, a removed one it
/// does not. `sync` closes exactly that.
#[tokio::test]
async fn sync_closes_the_gap_a_reingest_leaves() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();

    // The tree moves on: one file appears, one changes, one goes.
    std::fs::write(tree.path().join("notes/new.md"), "a brand new claim\n").unwrap();
    std::fs::write(
        tree.path().join("notes/mounts.md"),
        "A mount is where a kernel answers, and it has been rewritten.\n",
    )
    .unwrap();
    std::fs::remove_file(tree.path().join("notes/ownership.md")).unwrap();

    // A re-ingest gets two of the three right, which is why `sync` exists.
    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    let stale = execs
        .invoke(&call(&["search", "notes", "ownership"]), None)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&stale.stdout).contains("notes/ownership.md"),
        "the deleted file is still indexed after a re-ingest — the gap sync is for"
    );

    let out = execs
        .invoke(&call(&["sync", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        // The re-ingest above already wrote both, so an incremental sync reads neither.
        "synced 0 file(s), 2 unchanged, removed 1 document(s)\n"
    );

    // Gone.
    let gone = execs
        .invoke(&call(&["search", "notes", "ownership"]), None)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&gone.stdout).contains("no matches"),
        "{:?}",
        String::from_utf8_lossy(&gone.stdout)
    );
    // Added.
    let added = execs
        .invoke(&call(&["search", "notes", "brand new claim"]), None)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&added.stdout).contains("notes/new.md"),
        "{:?}",
        String::from_utf8_lossy(&added.stdout)
    );
    // Changed, and only once.
    let changed = execs
        .invoke(&call(&["search", "notes", "rewritten"]), None)
        .await
        .unwrap();
    let said = String::from_utf8_lossy(&changed.stdout);
    assert_eq!(said.matches("notes/mounts.md").count(), 1, "{said}");
}

/// A directory that is gone entirely means an empty tree under it, so what the index held for
/// it goes. That is the reading a caller wants when a whole subtree was deleted.
#[tokio::test]
async fn syncing_a_path_that_is_gone_empties_it() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    std::fs::remove_dir_all(tree.path().join("notes")).unwrap();

    let out = execs
        .invoke(&call(&["sync", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "synced 0 file(s), 0 unchanged, removed 2 document(s)\n"
    );
}

/// `sync` is scoped to the paths it was given, so it does not touch what another `ingest` put
/// in — the property that makes it safe to run on one subtree of a shared store.
#[tokio::test]
async fn sync_leaves_what_lies_outside_its_paths() {
    let (tree, root) = fixture();
    std::fs::create_dir(tree.path().join("other")).unwrap();
    std::fs::write(tree.path().join("other/keep.md"), "kept\n").unwrap();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(
            &call(&["ingest", "notes", "/notes", "/other"]),
            Some(&mount),
        )
        .await
        .unwrap();
    std::fs::remove_dir_all(tree.path().join("notes")).unwrap();

    execs
        .invoke(&call(&["sync", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();

    let listed = execs.invoke(&call(&["list"]), None).await.unwrap();
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("notes\t1 document(s)"),
        "other/keep.md survived: {:?}",
        String::from_utf8_lossy(&listed.stdout)
    );
}

/// Help is for whoever runs the name, so nothing in it is about how this is written.
///
/// A doc comment on a clap type *is* the help text, which makes it easy to leave a note for a
/// maintainer where a caller will read it — and clap takes a doc comment's first line as
/// `about` and the rest as `long_about`, so such a note quietly outranks the `about` set
/// beside it.
#[tokio::test]
async fn help_says_nothing_about_how_this_is_implemented() {
    let root = tempfile::tempdir().unwrap();
    let execs = registered(root.path()).unwrap();

    // Names that could only have come from the code. Not a Rust keyword that is also an
    // ordinary word — `match` is in "make a store match the tree", which is the help doing its
    // job rather than leaking anything.
    let leaks = [
        "PathBuf",
        "Vec<",
        "ExecCall",
        "clap",
        "The parse of one call",
    ];
    for args in [
        vec!["--help"],
        vec!["ingest", "--help"],
        vec!["search", "--help"],
        vec!["sync", "--help"],
        vec!["purge", "--help"],
        vec!["list", "--help"],
        vec!["drop", "--help"],
    ] {
        let out = execs.invoke(&call(&args), None).await.unwrap();
        assert_eq!(out.exit_code, 0, "{args:?}");
        let said = String::from_utf8_lossy(&out.stdout);
        for leak in leaks {
            assert!(
                !said.contains(leak),
                "{args:?} help mentions `{leak}`:\n{said}"
            );
        }
    }
}

/// The stores are on the top-level help too, which is where an agent looks first.
#[tokio::test]
async fn the_top_level_help_lists_the_stores() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    let before = execs.invoke(&call(&["--help"]), None).await.unwrap();
    assert!(
        !String::from_utf8_lossy(&before.stdout).contains("stores:"),
        "nothing to list yet"
    );

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();

    let after = execs.invoke(&call(&["--help"]), None).await.unwrap();
    assert!(
        String::from_utf8_lossy(&after.stdout).contains("stores: notes"),
        "{:?}",
        String::from_utf8_lossy(&after.stdout)
    );
}

/// The point of an incremental sync: a tree that did not move is not read again.
#[tokio::test]
async fn sync_reads_only_what_moved() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();

    let still = execs
        .invoke(&call(&["sync", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&still.stdout),
        "synced 0 file(s), 2 unchanged, removed 0 document(s)\n"
    );

    std::fs::write(tree.path().join("notes/mounts.md"), "rewritten entirely\n").unwrap();
    let moved = execs
        .invoke(&call(&["sync", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&moved.stdout),
        "synced 1 file(s), 1 unchanged, removed 0 document(s)\n",
        "only the file that moved is read"
    );
}

/// `--force` is the way out when a stamp cannot tell: an edit inside one timestamp tick that
/// kept the file's length.
#[tokio::test]
async fn force_reads_everything() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    let out = execs
        .invoke(&call(&["sync", "notes", "/notes", "--force"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "synced 2 file(s), 0 unchanged, removed 0 document(s)\n"
    );
}

/// A file that cannot be read is an absent file, not a file to leave the old document for.
///
/// A dangling symlink is the case: it has an indexable name, so a walk that judged by name
/// alone would keep the document it once had, and a search would answer with the body of a
/// file nothing can open.
#[tokio::test]
async fn sync_removes_a_document_whose_file_became_unreadable() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();

    let gone = tree.path().join("notes/ownership.md");
    std::fs::remove_file(&gone).unwrap();
    std::os::unix::fs::symlink("/nonexistent/target", &gone).unwrap();

    let out = execs
        .invoke(&call(&["sync", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "synced 0 file(s), 1 unchanged, removed 1 document(s)\n"
    );

    let found = execs
        .invoke(&call(&["search", "notes", "ownership"]), None)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&found.stdout).contains("no matches"),
        "the body of an unopenable file is still searchable: {:?}",
        String::from_utf8_lossy(&found.stdout)
    );
}

/// Reading must not take a write lock. tantivy allows one `IndexWriter` per directory, so a
/// store built eagerly with one would make every `search` fail while anything was ingesting,
/// and would lock out any other process for as long as a console lived.
///
/// Two `Index` values over one root stand in for two processes: they share no cache, so the
/// only thing that could couple them is the lock on the directory itself.
#[tokio::test]
async fn reading_does_not_take_the_write_lock() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let writing = registered(root.path()).unwrap();
    let reading = registered(root.path()).unwrap();

    writing
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();

    // The writer is now held by `writing`, for the life of that `Index`.
    let found = reading
        .invoke(&call(&["search", "notes", "kernel"]), None)
        .await
        .unwrap();
    assert_eq!(
        found.exit_code,
        0,
        "search while another holds the writer: {}",
        String::from_utf8_lossy(&found.stderr)
    );
    assert!(String::from_utf8_lossy(&found.stdout).contains("notes/mounts.md"));

    let listed = reading.invoke(&call(&["list"]), None).await.unwrap();
    assert_eq!(
        listed.exit_code,
        0,
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(String::from_utf8_lossy(&listed.stdout).contains("notes\t2 document(s)"));
}

/// A second writer is still refused, which is tantivy's rule and not something to paper over.
#[tokio::test]
async fn a_second_writer_is_refused_rather_than_silently_second() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let first = registered(root.path()).unwrap();
    let second = registered(root.path()).unwrap();

    first
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    let out = second
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    assert_ne!(out.exit_code, 0, "two writers on one index");
}

/// A listing that could not read a store says so in its exit code, so a caller asking what it
/// has does not read a broken store as an answer.
#[tokio::test]
async fn list_reports_an_unreadable_store_in_its_exit_code() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    // A directory that is not an index, wearing a store's name.
    std::fs::create_dir(root.path().join("broken")).unwrap();
    std::fs::write(root.path().join("broken/meta.json"), "not an index").unwrap();

    let out = execs.invoke(&call(&["list"]), None).await.unwrap();
    assert_eq!(out.exit_code, 1, "a broken store is not a clean listing");
    let listed = String::from_utf8_lossy(&out.stdout);
    assert!(listed.contains("notes\t2 document(s)"), "{listed}");
    assert!(
        listed.contains("broken"),
        "the broken one is still named: {listed}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("broken"),
        "and the reason is on stderr"
    );
}

/// `notes/` parses as one component, so a check that only walks components would take a
/// spelling that is a path.
#[tokio::test]
async fn a_store_name_with_a_separator_is_refused() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    let out = execs
        .invoke(&call(&["ingest", "notes/", "/notes"]), Some(&mount))
        .await
        .unwrap();
    assert_ne!(out.exit_code, 0, "{}", String::from_utf8_lossy(&out.stdout));
    assert!(!root.path().join("notes").exists());
}

/// A file too large to hold in memory is named rather than skipped in silence.
#[tokio::test]
async fn a_file_past_the_size_cap_is_reported() {
    let (tree, root) = fixture();
    std::fs::write(
        tree.path().join("notes/huge.md"),
        vec![b'a'; 9 * 1024 * 1024],
    )
    .unwrap();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    let out = execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    assert_eq!(out.exit_code, 0, "the other files still went in");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.starts_with("indexed 2 file(s)"), "{said}");
    assert!(
        said.contains("notes/huge.md"),
        "the one left out is named: {said}"
    );
}

/// Two arguments that overlap walk the same file twice. The documents come out right either
/// way, because a write is a delete and an add; the count is what lies.
#[tokio::test]
async fn overlapping_paths_are_counted_once() {
    let (tree, root) = fixture();
    std::fs::create_dir(tree.path().join("notes/sub")).unwrap();
    std::fs::write(tree.path().join("notes/sub/deep.md"), "under both\n").unwrap();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    let out = execs
        .invoke(
            &call(&["ingest", "notes", "/notes", "/notes/sub"]),
            Some(&mount),
        )
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "indexed 3 file(s)\n",
        "three files, one of them reachable two ways"
    );

    let listed = execs.invoke(&call(&["list"]), None).await.unwrap();
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("notes\t3 document(s)"),
        "the report and the store agree: {:?}",
        String::from_utf8_lossy(&listed.stdout)
    );
}

/// One rule for every file this declines to read: it is an absent file.
///
/// A file that grows past the cap is the case a size limit introduces, and it has to end the
/// same way a dangling symlink does. Otherwise the index asserts the contents of a file it
/// has decided never to open.
#[tokio::test]
async fn a_file_that_grew_past_the_cap_loses_its_document() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    std::fs::write(
        tree.path().join("notes/ownership.md"),
        vec![b'x'; 9 * 1024 * 1024],
    )
    .unwrap();

    let out = execs
        .invoke(&call(&["sync", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("removed 1 document(s)"),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let found = execs
        .invoke(&call(&["search", "notes", "ownership"]), None)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&found.stdout).contains("no matches"),
        "the body of a file past the cap is still searchable: {:?}",
        String::from_utf8_lossy(&found.stdout)
    );
}

/// A file that was readable at the walk and is not at the read ends the same way.
#[tokio::test]
async fn a_file_that_stopped_being_text_loses_its_document() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    // Same name, same extension, bytes that are not UTF-8.
    std::fs::write(tree.path().join("notes/ownership.md"), [0xff, 0xfe, 0x00]).unwrap();

    let out = execs
        .invoke(&call(&["sync", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("removed 1 document(s)"),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let found = execs
        .invoke(&call(&["search", "notes", "ownership"]), None)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&found.stdout).contains("no matches"));
}

/// A limit of zero is a usage error, not a panic.
///
/// `TopDocs::with_limit` asserts on `0`, and the limit comes straight from a caller: typed as
/// `NonZeroUsize` it never reaches the collector, and clap says so in the shape it says
/// everything else. What this pins is that a delegated call cannot be made to panic inside
/// tantivy by an argument.
#[tokio::test]
async fn a_limit_of_zero_is_a_usage_error() {
    let (tree, root) = fixture();
    let mount = Mounted(tree.path().to_path_buf());
    let execs = registered(root.path()).unwrap();

    execs
        .invoke(&call(&["ingest", "notes", "/notes"]), Some(&mount))
        .await
        .unwrap();

    let out = execs
        .invoke(&call(&["search", "notes", "-n", "0", "kernel"]), None)
        .await
        .unwrap();
    assert_eq!(out.exit_code, 2, "{}", String::from_utf8_lossy(&out.stderr));
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("0"), "{said}");
    assert!(
        !said.contains("panicked") && !said.contains("tantivy"),
        "an argument reached a collector: {said}"
    );

    // And the search still works either side of it.
    let ok = execs
        .invoke(&call(&["search", "notes", "-n", "1", "kernel"]), None)
        .await
        .unwrap();
    assert_eq!(ok.exit_code, 0, "{}", String::from_utf8_lossy(&ok.stderr));
}
