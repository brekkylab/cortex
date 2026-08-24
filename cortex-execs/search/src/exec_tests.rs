//! Unit tests for the command.
//!
//! No backend here reaches anything: fake ones hand back fixtures, because what these assert is
//! what the command decides — which stores get asked, what the report says, where the mount
//! prefix goes, and which exit code a caller reads. A backend's own answers are its own tests.

use futures_core::future::BoxFuture;

use super::*;
use crate::{Hit, SearchResult, Searchable};

/// A backend that answers with whatever it was built with.
struct Fake {
    hits: Vec<(&'static str, &'static str)>,
    refuse: Option<&'static str>,
    describe: &'static str,
}

impl Fake {
    fn with(hits: &[(&'static str, &'static str)]) -> Self {
        Fake {
            hits: hits.to_vec(),
            refuse: None,
            describe: "",
        }
    }

    fn refusing(why: &'static str) -> Self {
        Fake {
            hits: Vec::new(),
            refuse: Some(why),
            describe: "",
        }
    }

    fn described(mut self, what: &'static str) -> Self {
        self.describe = what;
        self
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

fn call(args: &[&str]) -> ExecCall {
    ExecCall {
        name: "search".into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        env: Default::default(),
    }
}

async fn run(search: Search, args: &[&str]) -> ExecResult {
    search.exec(&call(args), None).await
}

fn out(r: &ExecResult) -> String {
    String::from_utf8(r.stdout.clone()).unwrap()
}

fn said(r: &ExecResult) -> String {
    String::from_utf8(r.stderr.clone()).unwrap()
}

#[tokio::test]
async fn a_hit_arrives_under_the_path_its_store_is_mounted_at() {
    let search = Search::new().with(
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
    let search = Search::new()
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
    let search = Search::new()
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
    let search = Search::new()
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
        let search = Search::new()
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
    let search = Search::new().with("chat/slack", Fake::with(&[("a", "{}")]));
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
    let search = Search::new()
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
    let search = Search::new()
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
    let search = Search::new()
        .with("chat/slack", Fake::refusing("expired token"))
        .with("mail/work", Fake::with(&[("b", "{}")]));
    let r = run(search, &["가격"]).await;
    assert_eq!(r.exit_code, 0);
    assert_eq!(out(&r), "mail/work/b\t{}\n");
    assert!(said(&r).contains("could not search: expired token"));
}

#[tokio::test]
async fn nothing_registered_says_so_rather_than_finding_nothing() {
    let r = run(Search::new(), &["가격"]).await;
    assert_eq!(r.exit_code, 2);
    assert!(said(&r).contains("no store has an index"), "{}", said(&r));
}

#[tokio::test]
async fn help_needs_no_store_and_no_credential() {
    let r = run(Search::new(), &["--help"]).await;
    assert_eq!(r.exit_code, 0);
    let text = out(&r);
    assert!(text.contains("search <query>"), "{text}");
    assert!(text.contains("Exit codes follow grep"), "{text}");
}

#[tokio::test]
async fn a_query_is_required_and_a_count_has_to_be_a_number() {
    let search = || Search::new().with("chat/slack", Fake::with(&[("a", "{}")]));

    let r = run(search(), &["--count", "5"]).await;
    assert_eq!(r.exit_code, 2);
    assert!(said(&r).contains("no query"), "{}", said(&r));

    let r = run(search(), &["--count", "many", "가격"]).await;
    assert_eq!(r.exit_code, 2);
    assert!(said(&r).contains("not a number"), "{}", said(&r));
}
