//! `<name> search [-n N] <query>...` — ask the index.

use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::sync::Arc;

use cortex::exec::ExecResult;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::Value as _;
use tantivy::snippet::SnippetGenerator;

use crate::store::{Store, other};

/// How many hits a search answers with when nothing says otherwise.
///
/// `NonZeroUsize` because tantivy's `TopDocs::with_limit` asserts on `0`, and the limit comes
/// from a caller: parsed as this, `-n 0` is a usage error clap renders rather than a panic
/// from inside a collector.
pub(crate) const DEFAULT_LIMIT: NonZeroUsize = NonZeroUsize::new(10).expect("10 is not zero");
/// How much of a body a hit shows. Enough to judge a result by, short enough that ten of
/// them still fit in something an agent will read.
///
/// tantivy's `set_max_num_chars` is documented in characters and implemented against byte
/// offsets, so an anchored fragment of Korean or Japanese comes back about a third this long.
/// Shorter than asked is the harmless direction, and the match is inside it either way.
const SNIPPET: usize = 240;

/// One answer: where it is, how well it matched, and enough of it to tell.
pub struct Hit {
    pub path: String,
    pub title: String,
    pub score: f32,
    /// The body around what was queried — see [`excerpt`].
    pub snippet: String,
}

/// Search what [`ingest`](crate::ingest) has put in.
///
/// Takes no mount: the corpus was read at ingest time, and a query is answered from the
/// index alone. `query` arrives already joined — a shell has split the words, and
/// `<name> search rust ownership` is what a caller writes when they mean the phrase.
pub(crate) async fn run(store: &Arc<Store>, limit: NonZeroUsize, query: &str) -> ExecResult {
    let store = store.clone();
    let query = query.to_owned();
    match tokio::task::spawn_blocking(move || query_for(&store, &query, limit)).await {
        Ok(Ok(hits)) => ExecResult::ok(render(&hits)),
        Ok(Err(e)) => ExecResult::failed(1, format!("search: {e}\n")),
        Err(e) => ExecResult::failed(1, format!("search: {e}\n")),
    }
}

fn query_for(store: &Store, query: &str, limit: NonZeroUsize) -> std::io::Result<Vec<Hit>> {
    let fields = store.fields();
    let searcher = store.searcher()?;

    let parser = QueryParser::for_index(store.index(), vec![fields.title, fields.body]);
    // Lenient: the query comes from an agent that was not told this crate's syntax, and a
    // stray `:` or unbalanced quote should narrow the answer rather than refuse to give one.
    let (query, _warnings) = parser.parse_query_lenient(query);

    // `order_by_score` and not the bare `TopDocs`: in tantivy 0.26 `TopDocs` is the *builder*
    // and the terminal method is what implements `Collector`.
    let found: Vec<(tantivy::Score, tantivy::DocAddress)> = searcher
        .search(&query, &TopDocs::with_limit(limit.get()).order_by_score())
        .map_err(other)?;

    // One generator for the whole answer: it is built from the query's terms and this
    // searcher's document frequencies, and neither changes from one hit to the next.
    let mut around = SnippetGenerator::create(&searcher, &*query, fields.body).map_err(other)?;
    around.set_max_num_chars(SNIPPET);

    let mut hits = Vec::with_capacity(found.len());
    for (score, address) in found {
        let doc: tantivy::TantivyDocument = searcher.doc(address).map_err(other)?;
        let text = |field| {
            doc.get_first(field)
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let body: String = text(fields.body);
        hits.push(Hit {
            path: text(fields.path),
            title: text(fields.title),
            score,
            snippet: excerpt(&around, &body),
        });
    }
    Ok(hits)
}

/// The part of a body worth showing: **the query's neighbourhood**, or the opening when the
/// body is not what matched.
///
/// The head of a document says nothing about why the document is here — a hit on line 800
/// would show its title page and a score with no visible relation to each other. tantivy cuts
/// around the terms instead, on token boundaries, so the slice is valid UTF-8 without this
/// having to count characters to find a safe one.
///
/// A hit that matched on `title` alone has no `body` term to anchor to, and tantivy answers
/// that with a snippet holding no highlights. That is the signal to fall back to [`head`]
/// rather than to show nothing: the file is still an answer, it is just named like one.
fn excerpt(around: &SnippetGenerator, body: &str) -> String {
    let found = around.snippet(body);
    let Some(matched) = found.highlighted().first() else {
        return head(body);
    };
    // tantivy's window is where its budget ran out, not where the match is — the term can
    // sit against either edge of it, and against the right one the sentence it is in gets
    // cut off. So the window is only used to *find* the match; the excerpt is recut from the
    // body around it, which is what puts context on both sides.
    //
    // `find` because `Snippet` reports its highlights against the fragment and never says
    // where the fragment came from. Text that repeats can match an earlier copy of itself,
    // which shows a different instance of the same neighbourhood — not a wrong answer.
    let Some(base) = body.find(found.fragment()) else {
        return head(body);
    };
    around_match(body, base + matched.start, base + matched.end)
}

/// [`SNIPPET`] characters of `body` with `start..end` in the middle of them.
fn around_match(body: &str, start: usize, end: usize) -> String {
    let spare = SNIPPET.saturating_sub(body[start..end].chars().count()) / 2;
    // Stepped by `char_indices` rather than by bytes, so every edge is a character boundary
    // by construction instead of by a check — and the budget stays in characters, which is
    // what "enough to read" is measured in.
    let from = match spare.checked_sub(1) {
        Some(back) => body[..start]
            .char_indices()
            .rev()
            .nth(back)
            .map_or(0, |(at, _)| at),
        None => start,
    };
    let to = body[end..]
        .char_indices()
        .nth(spare)
        .map_or(body.len(), |(at, _)| end + at);

    // An ellipsis only where something was actually dropped. A newline the cut fell just
    // inside of is not text going on above.
    let before = if body[..from].trim().is_empty() {
        ""
    } else {
        "…"
    };
    let after = if body[to..].trim().is_empty() {
        ""
    } else {
        "…"
    };
    format!("{before}{}{after}", flatten(&body[from..to]))
}

/// The head of a body, for a hit the body did not match.
///
/// Cut by `char` and not by byte, or a multibyte character at the boundary panics — which is
/// most of a corpus that is not English.
fn head(body: &str) -> String {
    let head: String = body.chars().take(SNIPPET).collect();
    if body.chars().count() > SNIPPET {
        format!("{}…", flatten(&head))
    } else {
        flatten(&head)
    }
}

/// One line, whatever the file did with newlines and runs of spaces. The answer is
/// line-oriented, so a snippet that kept its own newlines would be several rows of it.
fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Score, path, then the snippet indented under it.
///
/// Line-oriented and tab-separated on purpose: this goes to a pipe, so it should survive
/// `cut` and `grep` as well as being read.
fn render(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "no matches\n".into();
    }
    let mut out = String::new();
    for hit in hits {
        let _ = writeln!(out, "{:.3}\t{}\t{}", hit.score, hit.path, hit.title);
        let _ = writeln!(out, "\t{}", hit.snippet);
    }
    out
}
