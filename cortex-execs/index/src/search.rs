//! `<name> search [-n N] <query>...` — ask the index.

use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::sync::Arc;

use cortex::exec::ExecResult;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::Value as _;

use crate::store::{Store, other};

/// How many hits a search answers with when nothing says otherwise.
///
/// `NonZeroUsize` because tantivy's `TopDocs::with_limit` asserts on `0`, and the limit comes
/// from a caller: parsed as this, `-n 0` is a usage error clap renders rather than a panic
/// from inside a collector.
pub(crate) const DEFAULT_LIMIT: NonZeroUsize = NonZeroUsize::new(10).expect("10 is not zero");
/// How much of a body a hit shows. Enough to judge a result by, short enough that ten of
/// them still fit in something an agent will read.
const SNIPPET: usize = 240;

/// One answer: where it is, how well it matched, and enough of it to tell.
pub struct Hit {
    pub path: String,
    pub title: String,
    pub score: f32,
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
            snippet: snippet(&body),
        });
    }
    Ok(hits)
}

/// The head of a body, on one line.
///
/// Cut by `char` and not by byte, or a multibyte character at the boundary panics — which is
/// most of a corpus that is not English.
fn snippet(body: &str) -> String {
    let head: String = body.chars().take(SNIPPET).collect();
    let flat = head.split_whitespace().collect::<Vec<_>>().join(" ");
    if body.chars().count() > SNIPPET {
        format!("{flat}…")
    } else {
        flat
    }
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
