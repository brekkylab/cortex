//! Unit tests for the command.
//!
//! Nothing here reaches anything: fake stores hand back fixtures, because what these assert is
//! what the command decides — which stores get asked, what the report says, where the mount
//! prefix goes, and which exit code a caller reads. A store's own answers are its own tests.
//!
//! The stores are *mounted* rather than registered, because that is the only way in: `Search`
//! reads the mount table, so a test that handed it a path directly would be testing a door that
//! does not exist.

use std::io;
use std::path::Path;

use cortex::fs::{Dirent, FileSystem, Hit, SearchResult, Searchable, Stat, WorkFs};
use futures_core::future::BoxFuture;

use super::*;

/// A store that answers its index with whatever it was built with, and answers everything else
/// with nothing — no test here opens a file.
struct Fake {
    hits: Vec<(&'static str, &'static str)>,
    refuse: Option<&'static str>,
    describe: &'static str,
    /// Whether this store has an index at all, which is not the same as having one that
    /// refuses: a store with none is never asked, and never appears in the report.
    indexed: bool,
}

impl Fake {
    fn with(hits: &[(&'static str, &'static str)]) -> Self {
        Fake {
            hits: hits.to_vec(),
            refuse: None,
            describe: "",
            indexed: true,
        }
    }

    fn refusing(why: &'static str) -> Self {
        Fake {
            refuse: Some(why),
            ..Fake::with(&[])
        }
    }

    /// A store like any other, with nothing to ask. A passthrough directory, an object store.
    fn unindexed() -> Self {
        Fake {
            indexed: false,
            ..Fake::with(&[])
        }
    }

    fn described(mut self, what: &'static str) -> Self {
        self.describe = what;
        self
    }
}

impl FileSystem for Fake {
    fn stat<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
        Box::pin(async { Err(io::ErrorKind::NotFound.into()) })
    }

    fn list<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn read_at<'a>(
        &'a self,
        _path: &'a Path,
        _buf: &'a mut [u8],
        _offset: u64,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async { Err(io::ErrorKind::NotFound.into()) })
    }

    fn index(&self) -> Option<&dyn Searchable> {
        self.indexed.then_some(self as &dyn Searchable)
    }
}

impl Searchable for Fake {
    fn search<'a>(&'a self, _query: &'a str, _count: usize) -> BoxFuture<'a, SearchResult> {
        Box::pin(async move {
            match self.refuse {
                Some(why) => Err(why.to_string()),
                None => Ok(self
                    .hits
                    .iter()
                    .map(|(path, record)| Hit {
                        path: (*path).to_string(),
                        record: format!("{record}\n").into_bytes(),
                    })
                    .collect()),
            }
        })
    }

    fn describe(&self) -> &str {
        self.describe
    }
}

/// A workspace under construction, so a test reads like the wiring it stands for.
///
/// `with` is `WorkFs::mount` and nothing else — the point of the test double is that there is
/// no second way to put a store in front of `Search`.
struct Work(WorkFs);

impl Work {
    fn new() -> Self {
        Work(WorkFs::new())
    }

    fn with<S: FileSystem + 'static>(mut self, at: &str, store: S) -> Self {
        self.0.mount(at, store).expect("mounted");
        self
    }
}

fn call(args: &[&str]) -> ExecCall {
    ExecCall {
        name: "search".into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        env: Default::default(),
    }
}

async fn run(work: Work, args: &[&str]) -> ExecResult {
    Search::over(&work.0).exec(&call(args), None).await
}

fn out(r: &ExecResult) -> String {
    String::from_utf8(r.stdout.clone()).unwrap()
}

fn said(r: &ExecResult) -> String {
    String::from_utf8(r.stderr.clone()).unwrap()
}

#[tokio::test]
async fn a_hit_arrives_under_the_path_its_store_is_mounted_at() {
    let search = Work::new().with(
        "chat/slack",
        Fake::with(&[(
            "channels/pricing__C1/2026/08/2026-08-03.jsonl",
            "{\"text\":\"가격\"}",
        )]),
    );
    let r = run(search, &["가격"]).await;
    assert_eq!(
        out(&r),
        "chat/slack/channels/pricing__C1/2026/08/2026-08-03.jsonl\t{\"text\":\"가격\"}\n"
    );
    assert_eq!(r.exit_code, 0);
}

/// The division the whole design rests on: a pipeline must see hits and nothing else, so the
/// report about which stores were asked cannot be on stdout.
#[tokio::test]
async fn the_report_is_on_stderr_and_the_hits_on_stdout() {
    let search = Work::new()
        .with("chat/slack", Fake::with(&[("a/chat.jsonl", "{}")]))
        .with("docs/notion", Fake::with(&[]).described("titles only"));
    let r = run(search, &["가격"]).await;

    assert_eq!(
        out(&r),
        "chat/slack/a/chat.jsonl\t{}\n",
        "stdout is hits only"
    );
    let report = said(&r);
    assert!(report.contains("# chat/slack  1 hit\n"), "{report}");
    assert!(
        report.contains("# docs/notion  0 hits (titles only)"),
        "an index that answered nothing says what it looks at: {report}"
    );
}

#[tokio::test]
async fn without_a_scope_every_store_is_asked() {
    let search = Work::new()
        .with("chat/slack", Fake::with(&[("a", "{}")]))
        .with("mail/work", Fake::with(&[("b", "{}")]));
    let r = run(search, &["가격"]).await;
    let text = out(&r);
    assert!(text.contains("chat/slack/a\t"), "{text}");
    assert!(text.contains("mail/work/b\t"), "{text}");
}

/// `--in` is a prefix and not a name: narrowing is what it is for, and a reader thinks in paths.
#[tokio::test]
async fn a_scope_narrows_by_prefix() {
    let search = Work::new()
        .with("chat/slack", Fake::with(&[("a", "{}")]))
        .with("chat/discord", Fake::with(&[("b", "{}")]))
        .with("mail/work", Fake::with(&[("c", "{}")]));

    let r = run(search, &["--in", "chat", "가격"]).await;
    let text = out(&r);
    assert!(
        text.contains("chat/slack/a\t") && text.contains("chat/discord/b\t"),
        "{text}"
    );
    assert!(!text.contains("mail/work"), "the scope excluded it: {text}");
}

/// The root is the prefix rule's one exception: it is a prefix of every path, and trimming its
/// only character leaves nothing for a comparison to match. Spelled either way it has to mean
/// every store — reading as "no store mounted under " is the silent-nothing this crate refuses.
#[tokio::test]
async fn the_root_scope_is_every_store() {
    for spelling in ["/", "//", ""] {
        let search = Work::new()
            .with("chat/slack", Fake::with(&[("a", "{}")]))
            .with("mail/work", Fake::with(&[("b", "{}")]));

        let r = run(search, &["--in", spelling, "가격"]).await;
        let text = out(&r);
        assert_eq!(r.exit_code, 0, "--in {spelling:?}: {}", said(&r));
        assert!(
            text.contains("chat/slack/a\t") && text.contains("mail/work/b\t"),
            "--in {spelling:?} reached both: {text}"
        );
    }
}

/// A typo must not read as "nothing there". The candidates are named, because the question the
/// caller asked was answerable and this was not the answer.
#[tokio::test]
async fn a_scope_nobody_serves_is_an_error_and_names_the_candidates() {
    let search = Work::new().with("chat/slack", Fake::with(&[("a", "{}")]));
    let r = run(search, &["--in", "chta/slack", "가격"]).await;
    assert_eq!(r.exit_code, 2);
    let report = said(&r);
    assert!(
        report.contains("no store mounted under chta/slack"),
        "{report}"
    );
    assert!(report.contains("chat/slack"), "{report}");
}

#[tokio::test]
async fn every_store_answered_and_none_had_it_is_exit_one() {
    let search = Work::new()
        .with("chat/slack", Fake::with(&[]))
        .with("mail/work", Fake::with(&[]));
    let r = run(search, &["없는말"]).await;
    assert_eq!(r.exit_code, 1);
    assert!(r.stdout.is_empty());
    assert!(
        said(&r).contains("0 hits"),
        "the report still says who was asked"
    );
}

/// The distinction the lane is built on, in one exit code: a store that could not look has not
/// said the thing is absent.
#[tokio::test]
async fn a_store_that_could_not_look_makes_nothing_found_exit_two() {
    let search = Work::new()
        .with("chat/slack", Fake::refusing("needs a user token"))
        .with("mail/work", Fake::with(&[]));
    let r = run(search, &["가격"]).await;
    assert_eq!(r.exit_code, 2);
    let report = said(&r);
    assert!(
        report.contains("could not search: needs a user token"),
        "{report}"
    );
}

/// But a refusal next to a hit is not a failure of the command: something was found, and the
/// refusal is reported beside it.
#[tokio::test]
async fn a_refusal_beside_a_hit_still_exits_zero_and_is_reported() {
    let search = Work::new()
        .with("chat/slack", Fake::refusing("expired token"))
        .with("mail/work", Fake::with(&[("b", "{}")]));
    let r = run(search, &["가격"]).await;
    assert_eq!(r.exit_code, 0);
    assert_eq!(out(&r), "mail/work/b\t{}\n");
    assert!(said(&r).contains("could not search: expired token"));
}

#[tokio::test]
async fn an_empty_workspace_says_so_rather_than_finding_nothing() {
    let r = run(Work::new(), &["가격"]).await;
    assert_eq!(r.exit_code, 2);
    assert!(said(&r).contains("nothing is mounted"), "{}", said(&r));
}

#[tokio::test]
async fn help_needs_no_store_and_no_credential() {
    let r = run(Work::new(), &["--help"]).await;
    assert_eq!(r.exit_code, 0);
    let text = out(&r);
    assert!(text.contains("search <query>"), "{text}");
    // Named rather than merely present: `--help` is where the exit codes are documented, and
    // they are close to `grep`'s without being `grep`'s — a store that could not be asked
    // leaves a zero exit here and a 2 there. A reader who assumes parity acts on a partial
    // answer, so the text has to say which it is.
    assert!(text.contains("Exit codes:"), "{text}");
    assert!(text.contains("not the same"), "{text}");
}

#[tokio::test]
async fn a_query_is_required_and_a_count_has_to_be_a_number() {
    let search = || Work::new().with("chat/slack", Fake::with(&[("a", "{}")]));

    let r = run(search(), &["--count", "5"]).await;
    assert_eq!(r.exit_code, 2);
    assert!(said(&r).contains("no query"), "{}", said(&r));

    let r = run(search(), &["--count", "many", "가격"]).await;
    assert_eq!(r.exit_code, 2);
    assert!(said(&r).contains("not a number"), "{}", said(&r));
}

/// A record is required to end its own line. The command terminates one that does not, because
/// the failure is invisible: two hits on one line is one line to `cut -f1`, and every hit after
/// the first is gone with a zero exit and nothing in the report.
#[tokio::test]
async fn a_record_that_does_not_end_its_line_still_gets_one() {
    struct Blunt;
    impl FileSystem for Blunt {
        fn stat<'a>(&'a self, _p: &'a Path) -> BoxFuture<'a, io::Result<Stat>> {
            Box::pin(async { Err(io::ErrorKind::NotFound.into()) })
        }
        fn list<'a>(&'a self, _p: &'a Path) -> BoxFuture<'a, io::Result<Vec<Dirent>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn read_at<'a>(
            &'a self,
            _p: &'a Path,
            _b: &'a mut [u8],
            _o: u64,
        ) -> BoxFuture<'a, io::Result<usize>> {
            Box::pin(async { Err(io::ErrorKind::NotFound.into()) })
        }
        fn index(&self) -> Option<&dyn Searchable> {
            Some(self)
        }
    }
    impl Searchable for Blunt {
        fn search<'a>(&'a self, _q: &'a str, _n: usize) -> BoxFuture<'a, SearchResult> {
            Box::pin(async move {
                Ok(vec![
                    Hit { path: "a".into(), record: b"{\"t\":1}".to_vec() },
                    Hit { path: "b".into(), record: b"{\"t\":2}".to_vec() },
                ])
            })
        }
    }

    let r = run(Work::new().with("chat/slack", Blunt), &["가격"]).await;
    let text = out(&r);
    assert_eq!(
        text.lines().count(),
        2,
        "two hits are two lines, whatever the backend terminated: {text:?}"
    );
    // And the first field of each line is still a path a reader can open.
    let first: Vec<&str> = text.lines().filter_map(|l| l.split('\t').next()).collect();
    assert_eq!(first, ["chat/slack/a", "chat/slack/b"], "{text:?}");
}

/// A query with no content in it is not a search. Both spellings reach the parser as at least
/// one word, so the guard has to be about the joined query rather than the argument count.
#[tokio::test]
async fn a_query_with_no_content_is_refused_before_any_store_is_asked() {
    for argv in [vec![""], vec!["   "], vec!["", ""], vec!["\t"]] {
        let search = Work::new().with("chat/slack", Fake::with(&[("a", "{}")]));
        let r = run(search, &argv).await;
        assert_eq!(r.exit_code, 2, "{argv:?} reached a store: {}", out(&r));
        assert!(said(&r).contains("no query"), "{}", said(&r));
    }
}

/// The binary answers `--help` before it can build a store, so it has to ask the parser what
/// help means rather than scan for the word. Scanning is how the two came apart: `--in --help`
/// is help to a scan and a missing query to `exec`, and one command cannot answer twice.
#[test]
fn help_is_what_the_parser_says_it_is_and_not_what_a_scan_says() {
    let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    for asks in [vec!["--help"], vec!["-h"], vec!["가격", "--help"]] {
        assert!(wants_help(&argv(&asks)), "{asks:?}");
    }
    // `--help` in the position of a flag's value is that value, both here and in `exec`.
    for does_not in [
        vec!["--in", "--help"],
        vec!["--count", "--help"],
        vec!["--in", "-h"],
        vec!["가격"],
    ] {
        assert!(!wants_help(&argv(&does_not)), "{does_not:?}");
    }
}

/// The three states the report has to keep apart: found nothing, could not look, and has
/// nothing to look with. The third is the one a list of backends could not express — the store
/// was never handed over, so the fan-out could not name a place it had not heard of.
#[tokio::test]
async fn a_store_with_no_index_is_named_as_unsearched() {
    let r = run(
        Work::new()
            .with("chat/slack", Fake::with(&[("a", "{}")]))
            .with("files/s3", Fake::unindexed()),
        &["가격"],
    )
    .await;

    assert_eq!(r.exit_code, 0, "one store answered, so this is a find");
    let said = said(&r);
    assert!(said.contains("chat/slack  1 hit"), "{said}");
    assert!(
        said.contains("files/s3  no index"),
        "a place nobody looked has to say so: {said}"
    );
    assert!(
        !said.contains("files/s3  0 hits"),
        "and must not read as a place that answered: {said}"
    );
}

/// Having no index moves no exit code. It is a fact about the store, where a refusal is a
/// fault — so a workspace with one searchable store and five plain ones still answers 1 when
/// the searchable one simply has nothing.
#[tokio::test]
async fn no_index_is_not_a_failure() {
    let r = run(
        Work::new()
            .with("chat/slack", Fake::with(&[]))
            .with("files/s3", Fake::unindexed()),
        &["가격"],
    )
    .await;

    assert_eq!(r.exit_code, 1, "everything that could answer did, and none had it");
    assert!(said(&r).contains("files/s3  no index"), "{}", said(&r));
}

/// A scope that reaches only stores with no index is not an empty result: nothing was read, so
/// nothing may be reported absent. The reader named the place, so the answer is about it.
#[tokio::test]
async fn a_scope_with_no_index_in_it_is_an_error_and_points_at_grep() {
    let r = run(
        Work::new()
            .with("chat/slack", Fake::with(&[("a", "{}")]))
            .with("files/s3", Fake::unindexed()),
        &["--in", "files/s3", "가격"],
    )
    .await;

    assert_eq!(r.exit_code, 2, "not an empty result");
    let said = said(&r);
    assert!(said.contains("files/s3  no index"), "{said}");
    assert!(said.contains("nothing in scope has an index"), "{said}");
    assert!(said.contains("grep"), "it has to say what does work: {said}");
}

/// A prefix nobody mounted is still the older error, and it names what *is* mounted — including
/// the stores that have no index, because a typo is as likely to be one of those.
#[tokio::test]
async fn a_scope_nobody_mounted_names_what_is_mounted() {
    let r = run(
        Work::new()
            .with("chat/slack", Fake::with(&[("a", "{}")]))
            .with("files/s3", Fake::unindexed()),
        &["--in", "files/typo", "가격"],
    )
    .await;

    assert_eq!(r.exit_code, 2);
    let said = said(&r);
    assert!(said.contains("no store mounted under files/typo"), "{said}");
    assert!(said.contains("chat/slack"), "{said}");
    assert!(said.contains("files/s3"), "{said}");
}

/// The prefix on a hit is the mount table's own key, so there is no second spelling to drift.
#[tokio::test]
async fn the_prefix_is_the_path_the_store_was_mounted_at() {
    let r = run(
        Work::new().with("chat/acme", Fake::with(&[("channels/x__C1/2026/08/2026-08-03.jsonl", "{}")])),
        &["가격"],
    )
    .await;
    assert!(
        out(&r).starts_with("chat/acme/channels/x__C1/"),
        "{}",
        out(&r)
    );
}
