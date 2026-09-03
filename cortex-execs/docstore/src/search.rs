//! `<name> search [-n N] <query>...` — ask the store.

use std::fmt::Write as _;
use std::sync::Arc;

use cortex::exec::ExecResult;

use crate::store::{Hit, Store, as_expression};

/// How many hits a search answers with when nothing says otherwise.
pub(crate) const DEFAULT_LIMIT: usize = 10;

/// Search what [`ingest`](crate::ingest) has put in.
///
/// Takes no path and opens no file in the tree: the bytes were read when they were ingested,
/// and a query is answered from the store alone. `query` arrives already joined — a shell has
/// split the words, and `<name> search notes.db rust ownership` is what a caller writes when
/// they mean the phrase.
pub(crate) async fn run(store: Arc<Store>, limit: usize, query: &str) -> ExecResult {
    // A query with no words in it is answered here rather than asked of FTS5: the expression
    // would be empty, which is a syntax error, and "nothing is near this" is the truthful
    // answer to a question that has nothing in it.
    let Some(expression) = as_expression(query) else {
        return ExecResult::ok(render(&[]));
    };

    match tokio::task::spawn_blocking(move || store.search(&expression, limit)).await {
        Ok(Ok(hits)) => ExecResult::ok(render(&hits)),
        Ok(Err(e)) => ExecResult::failed(1, format!("search: {e}\n")),
        Err(e) => ExecResult::failed(1, format!("search: {e}\n")),
    }
}

/// Score, path, then the snippet indented under it.
///
/// Line-oriented and tab-separated on purpose: this goes to a pipe, so it should survive `cut`
/// and `grep` as well as being read. The shape is `cortex-exec-index`'s, so that something
/// driving either reads one kind of output.
fn render(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "no matches\n".into();
    }
    let mut out = String::new();
    for hit in hits {
        let _ = writeln!(out, "{:.3}\t{}\t{}", hit.score, hit.path, hit.title);
        let _ = writeln!(out, "\t{}", flatten(&hit.snippet));
    }
    out
}

/// One line, whatever the file did with newlines and runs of spaces. The answer is
/// line-oriented, so a snippet that kept its own newlines would be several rows of it.
fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
