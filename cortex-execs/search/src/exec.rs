//! The command: a query in, paths and records out, one report line per store asked.

use std::sync::Arc;

use cortex::exec::{ExecCall, ExecResult, Executable};
use cortex::fs::{FileSystem, Hit, Mount, WorkFs};
use futures_core::future::BoxFuture;

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
the hit as that file spells it. Which stores were asked, what each answered, and which were not
asked at all goes to stderr — so a pipeline sees only hits.

    # chat/slack   12 hits (messages)      asked, and here is what it had
    # docs/notion   0 hits (titles only)   asked, and it has none
    # mail/work    could not search: ...   asked, and it would not answer
    # files/s3      not searchable         nobody looked here — try grep

Examples

    search 'pricing policy'
    search 'in:#pricing from:@amy' --in chat/slack --count 50

    # the files to read next
    search 'pricing policy' | cut -f1 | sort -u

    # what an index cannot express: recall from the index, precision from the tree.
    # NUL-separated, because a name can hold a space and `xargs` splits on one; `-E`,
    # because `?` is a literal in the pattern language `grep` uses by default.
    search 'pricing' | cut -f1 | sort -u | tr '\\n' '\\0' | xargs -0 grep -nHE 'A/B ?test'

One search asks each store's index once; walking the tree asks per file. A day of chat costs a
call and each thread costs another, so `grep -r` over a mount is thousands of them and a rate
limit answers first. Search to find the files, then read those.

The index query is one request. Naming what comes back is not: a messenger store resolves author
names and conversation names from listings that page, so a cold search spends those too and holds
them for a few minutes. Search to find the files, then read those.

Exit codes: 0 found something, 1 every store answered and none had it, 2 nothing could be
searched — or something could not, and nothing was found. A store with no index moves none of
them: it is a fact about that store and not a fault, so it is named and then left out of the
arithmetic. Close to `grep`'s and not the same:
`grep` answers 2 when a file was unreadable even though another matched, where this answers 0 and
names the store it could not ask on stderr. A caller that must not act on a partial answer reads
the report, not the code.
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

        // Joined first, because emptiness is a property of the query and not of the argument
        // count: `search ""` and `search "" ""` both arrive as one or more words and both mean
        // nothing. Sending either on spends a request per store against the budget this command
        // exists to conserve, to ask a service a question with no content in it.
        let query = words.join(" ");
        if query.trim().is_empty() {
            return Err(usage_error("no query"));
        }
        Ok(Parsed::Run(Args {
            query,
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

/// Whether this argv is asking for help, by the same reading the command gives it.
///
/// Exists so a caller that has to answer `--help` *before* it can build the stores — the binary
/// does, because building them wants a credential and a question about the command is not a
/// question about a service — asks the parser rather than scanning for the word. Scanning is how
/// the two disagreed: `--in --help` is help to a scan and a missing query to the parser, and a
/// command that answers one way as a binary and another way through a console is two commands.
pub fn wants_help(args: &[String]) -> bool {
    matches!(Args::parse(args), Ok(Parsed::Help))
}

/// One store that has an index, under the path the workspace mounts it at.
struct Store {
    at: String,
    fs: Arc<dyn FileSystem>,
}

/// `search`, over the stores a workspace mounts.
///
/// Built from the mount table and from nothing else, which is the whole of why there is one
/// constructor. A hit's path is only openable because it is prefixed with where its store
/// actually is, so a path this command was told separately is a path that can disagree with the
/// tree — and a wrong prefix is indistinguishable downstream from a file that was deleted.
/// Taking both from [`WorkFs::indexes`] means there is no second spelling to keep in step.
pub struct Search {
    stores: Vec<Store>,
}

impl Search {
    /// `search` over every store in `work`.
    ///
    /// Mounting is the whole of the registration: a store that answers
    /// [`FileSystem::searchable`](cortex::fs::FileSystem::searchable) is searchable from the moment it is
    /// in the tree. Neither that nor its opposite is a thing to remember to do.
    ///
    /// Every mount is taken, not only the ones with an index, because the ones without are
    /// something a reader has to be told. "Nothing found here" and "nobody looked here" are
    /// different answers, and a fan-out that never learned a store existed can only give the
    /// first.
    ///
    /// The stores are held by [`Arc`] rather than borrowed, because this outlives the workspace
    /// value: a binding takes the `WorkFs` by value to mount it, so a `Search` that borrowed
    /// could not be registered in an [`ExecutableSet`](cortex::exec::ExecutableSet) and handed
    /// to a console. Build this before mounting and both halves work off the same table.
    pub fn over(work: &WorkFs) -> Self {
        Search {
            stores: work
                .stores()
                .into_iter()
                .map(|(at, fs)| Store {
                    // The mount table's own key, trimmed the way `--in` is so the two compare:
                    // a scope is written by a reader and a key is written by the table, and a
                    // leading slash on either would make one path two.
                    at: at.to_string_lossy().trim_matches('/').to_string(),
                    fs,
                })
                .collect(),
        }
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
            let mounted: Vec<&str> = self.stores.iter().map(|s| s.at.as_str()).collect();
            // Nothing is *mounted* there, which is a different sentence from nothing being
            // searchable there — that one is settled further down, after the stores in scope
            // have been asked. A typo must not read as "nothing there" either way, so the
            // candidates say what would have been reached.
            return Err(Failure {
                code: 2,
                message: match args.scope {
                    Some(p) if mounted.is_empty() => {
                        format!("search: nothing is mounted, so --in {p} reaches none\n")
                    }
                    Some(p) => format!(
                        "search: no store mounted under {p}\n       mounted: {}\n",
                        mounted.join(", ")
                    ),
                    None => "search: nothing is mounted\n".to_string(),
                },
            });
        }

        // Asked one at a time, deliberately. Two mounts can sit behind one credential — several
        // guilds of one bot, several workspaces of one Grid token — and those share a rate
        // limit, so a fan-out that raced would spend the budget it exists to save.
        let mut hits = Vec::new();
        let mut report = Vec::new();
        let mut refused = 0usize;
        let mut asked = 0usize;
        for store in &stores {
            // Three states here and not two, which is the whole of why the mount table is read
            // rather than a list of backends. A store with no index is not a failure and not an
            // empty answer: it is a place nobody looked, it says so on its own line, and it
            // moves no exit code — where a *refusal* below is a fault and forces one.
            let Some(index) = store.fs.searchable() else {
                report.extend_from_slice(format!("# {}  not searchable\n", store.at).as_bytes());
                continue;
            };
            asked += 1;
            match index.search(&args.query, args.count).await {
                Ok(found) => {
                    report.extend_from_slice(
                        format!(
                            "# {}  {} hit{}{}\n",
                            store.at,
                            found.len(),
                            if found.len() == 1 { "" } else { "s" },
                            match index.describe() {
                                "" => String::new(),
                                d => format!(" ({d})"),
                            }
                        )
                        .as_bytes(),
                    );
                    for hit in found {
                        hits.extend_from_slice(line(store, &hit).as_bytes());
                        hits.extend_from_slice(&hit.record);
                        // A record is required to end its own line and the shipped backend does.
                        // Terminated here anyway, because the alternative when one does not is
                        // two hits on one line — which `cut -f1` reads as one, losing the rest
                        // with no error and a zero exit. Not a check a caller could make either:
                        // the damage is in bytes it never sees.
                        if !hit.record.ends_with(b"\n") {
                            hits.push(b'\n');
                        }
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

        if asked == 0 {
            // Every store in scope was mounted and none had an index, so nothing was searched
            // at all. That is the same 2 an outright failure gets, and for the same reason:
            // answering 1 would say the thing is not there, about a place nobody read.
            let mut message = String::from_utf8_lossy(&report).into_owned();
            message.push_str(
                "search: nothing in scope is searchable\n                        this command asks a service to search; grep reads the files itself\n",
            );
            return Err(Failure { code: 2, message });
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
