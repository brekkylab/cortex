//! The command: a query in, paths and records out, one report line per store asked.

use cortex::exec::{ExecCall, ExecResult, Executable};
use cortex::fs::Mount;
use futures_core::future::BoxFuture;

use crate::{Hit, Searchable};

/// Hits per store when nobody says. Small on purpose: this is the step that decides *where to
/// look*, and a caller that wants more can ask for more.
const COUNT: usize = 20;

/// The most any one index is asked for. Past this a reader is enumerating rather than
/// searching, which the tree does better and cheaper.
const MAX_COUNT: usize = 100;

const USAGE: &str = "\
search — the indexes behind the mounted stores, as paths into this tree

    search <query> [--in PREFIX] [--count N]

<query> is the service's own search syntax, passed through untouched (Slack: `in:#channel`,
`from:@someone`, \"a quoted phrase\"). Without --in every store that has an index is asked, so
write plain words there: an operator one service understands is literal text to the next.

    --in PREFIX     only stores mounted under this path
    --count N       hits per store (default 20, most 100)
    --help          this

Results go to stdout, one hit per line: the path of the file it can be read in, a tab, then
the hit as that file spells it. Which stores were asked, and what each answered, goes to
stderr — so a pipeline sees only hits.

Examples

    search 'pricing policy'
    search 'in:#pricing from:@amy' --in chat/slack --count 50

    # the files to read next
    search 'pricing policy' | cut -f1 | sort -u

    # what an index cannot express: recall from the index, precision from the tree.
    # NUL-separated, because a name can hold a space and `xargs` splits on one; `-E`,
    # because `?` is a literal in the pattern language `grep` uses by default.
    search 'pricing' | cut -f1 | sort -u | tr '\\n' '\\0' | xargs -0 grep -nHE 'A/B ?test'

One search is one request per store; walking the tree is not. A day of chat costs a call and
each thread costs another, so `grep -r` over a mount is thousands of them and a rate limit
answers first. Search to find the files, then read those.

Exit codes follow grep: 0 found something, 1 every store answered and none had it, 2 nothing
could be searched — or something could not, and nothing was found.
";

/// What the command was asked to do.
struct Args {
    query: String,
    count: usize,
    scope: Option<String>,
}

enum Parsed {
    Help,
    Run(Args),
}

impl Args {
    /// Options anywhere, everything else the query.
    ///
    /// The query is joined from what is left rather than taken as one argument: a search is
    /// written the way it is typed, and a shell that split it on spaces has not changed what
    /// was meant.
    fn parse(argv: &[String]) -> Result<Parsed, Failure> {
        let mut words: Vec<&str> = Vec::new();
        let mut count = COUNT;
        let mut scope = None;
        let mut it = argv.iter();

        while let Some(a) = it.next() {
            match a.as_str() {
                "--help" | "-h" => return Ok(Parsed::Help),
                "--count" => {
                    let v = it
                        .next()
                        .ok_or_else(|| usage_error("--count needs a number"))?;
                    let n: usize = v
                        .parse()
                        .map_err(|_| usage_error(&format!("--count: not a number: {v}")))?;
                    if n == 0 {
                        return Err(usage_error("--count: zero hits is not a search"));
                    }
                    count = n.min(MAX_COUNT);
                }
                "--in" => {
                    let v = it.next().ok_or_else(|| usage_error("--in needs a path"))?;
                    // The root is the one path the prefix rule cannot express, because
                    // trimming its only character leaves nothing to compare against. It is
                    // also the prefix of every path there is, so `--in /` is the same request
                    // as no flag at all and is normalized to it here rather than reaching
                    // `selected` as a scope no store can match.
                    scope = match v.trim_matches('/') {
                        "" => None,
                        at => Some(at.to_string()),
                    };
                }
                other => words.push(other),
            }
        }

        if words.is_empty() {
            return Err(usage_error("no query"));
        }
        Ok(Parsed::Run(Args {
            query: words.join(" "),
            count,
            scope,
        }))
    }
}

/// An exit code and what to say on the way out.
struct Failure {
    code: i32,
    message: String,
}

fn usage_error(what: &str) -> Failure {
    Failure {
        code: 2,
        message: format!("search: {what}\n\n{USAGE}"),
    }
}

/// What the command says about itself, before there is anything to search with.
pub fn usage() -> &'static str {
    USAGE
}

/// One store, under the path it is mounted at.
struct Store {
    at: String,
    backend: Box<dyn Searchable>,
}

/// `search`, over the stores it was given.
///
/// The registry is built by whoever wired the session, because that is the only place that
/// knows what is mounted where — a mount table hands out paths, not the stores behind them.
#[derive(Default)]
pub struct Search {
    stores: Vec<Store>,
}

impl Search {
    pub fn new() -> Self {
        Search::default()
    }

    /// Register `backend` as the index for the store mounted at `at`.
    ///
    /// The path is what a reader will see in front of every hit from this backend, so it has to
    /// be the same path the store was mounted at.
    pub fn with(mut self, at: impl Into<String>, backend: impl Searchable + 'static) -> Self {
        self.stores.push(Store {
            at: at.into().trim_matches('/').to_string(),
            backend: Box::new(backend),
        });
        self
    }

    /// One line for [`ExecutableSet::register`](cortex::exec::ExecutableSet::register).
    pub fn summary() -> &'static str {
        "find things through the mounted stores' own indexes, as paths into this tree"
    }

    /// The stores `--in` names, or all of them.
    ///
    /// A prefix and not a name, so `--in chat` reaches every chat store: narrowing is the point
    /// of the flag and a reader thinks in paths, not in registrations. `--in /` names the root,
    /// which every store is under, and arrives here as `None` for that reason.
    fn selected(&self, scope: Option<&str>) -> Vec<&Store> {
        match scope {
            None => self.stores.iter().collect(),
            Some(p) => self
                .stores
                .iter()
                .filter(|s| s.at == p || s.at.starts_with(&format!("{p}/")))
                .collect(),
        }
    }

    /// The whole command, with everything that can go wrong in one place.
    async fn run(&self, call: &ExecCall) -> Result<(Vec<u8>, Vec<u8>), Failure> {
        let args = match Args::parse(&call.args)? {
            Parsed::Help => return Ok((USAGE.into(), Vec::new())),
            Parsed::Run(a) => a,
        };

        let stores = self.selected(args.scope.as_deref());
        if stores.is_empty() {
            let known: Vec<&str> = self.stores.iter().map(|s| s.at.as_str()).collect();
            // A typo must not read as "nothing there": the answer to a scope nobody serves is
            // that nobody serves it, and the candidates say what would have.
            return Err(Failure {
                code: 2,
                message: match args.scope {
                    Some(p) if known.is_empty() => {
                        format!("search: no store has an index, so --in {p} reaches none\n")
                    }
                    Some(p) => format!(
                        "search: no store mounted under {p}\n       searchable: {}\n",
                        known.join(", ")
                    ),
                    None => "search: no store has an index\n".to_string(),
                },
            });
        }

        // Asked one at a time, deliberately. Two mounts can sit behind one credential — several
        // guilds of one bot, several workspaces of one Grid token — and those share a rate
        // limit, so a fan-out that raced would spend the budget it exists to save.
        let mut hits = Vec::new();
        let mut report = Vec::new();
        let mut refused = 0usize;
        for store in &stores {
            match store.backend.search(&args.query, args.count).await {
                Ok(found) => {
                    report.extend_from_slice(
                        format!(
                            "# {}  {} hit{}{}\n",
                            store.at,
                            found.len(),
                            if found.len() == 1 { "" } else { "s" },
                            match store.backend.describe() {
                                "" => String::new(),
                                d => format!(" ({d})"),
                            }
                        )
                        .as_bytes(),
                    );
                    for hit in found {
                        hits.extend_from_slice(line(store, &hit).as_bytes());
                        hits.extend_from_slice(&hit.record);
                    }
                }
                Err(why) => {
                    refused += 1;
                    report.extend_from_slice(
                        format!("# {}  could not search: {why}\n", store.at).as_bytes(),
                    );
                }
            }
        }

        if hits.is_empty() {
            // Nothing found is `grep`'s 1 — but only if everything answered. A store that could
            // not look has not said the thing is absent, and reporting one as the other is the
            // silent failure this whole lane is built to refuse.
            return Err(Failure {
                code: if refused > 0 { 2 } else { 1 },
                message: String::from_utf8_lossy(&report).into_owned(),
            });
        }
        Ok((hits, report))
    }
}

/// `<mount>/<path>\t` — the half of the line that says where to read the rest.
fn line(store: &Store, hit: &Hit) -> String {
    if store.at.is_empty() {
        format!("{}\t", hit.path)
    } else {
        format!("{}/{}\t", store.at, hit.path)
    }
}

impl Executable for Search {
    /// The mount is ignored, and that is the point of the command: it names files rather than
    /// reading them. Confirming a path exists would fetch what is behind it, which is the cost
    /// this exists to avoid — the caller pays it for the hits it actually opens.
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        _mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecResult> {
        Box::pin(async move {
            match self.run(call).await {
                Ok((stdout, stderr)) => {
                    let mut out = ExecResult::ok(stdout);
                    out.stderr = stderr;
                    out
                }
                Err(f) => ExecResult::failed(f.code, f.message),
            }
        })
    }
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod exec_tests;
