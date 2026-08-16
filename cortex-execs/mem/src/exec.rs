//! `mem` as a command: arguments in, lines out.

use std::{io, sync::Arc};

use cortex::{
    executable::{ExecCall, ExecResult, Executable},
    fs::Mount,
};
use futures_core::future::BoxFuture;

use crate::{
    Embedder, Inference, Memory,
    store::{Applied, Entry, Hit, Record, Scope},
};

/// What `mem` prints when it is asked, and when it is misused.
pub const USAGE: &str = "\
mem — a memory store that is one file in this tree

usage:
  mem insert  <store> <text>   remember what is worth remembering in <text>
  mem search  <store> <query>  the memories nearest <query>, nearest first
  mem list    <store>          every memory, oldest first
  mem get     <store> <id>     one memory, by the id it was written under
  mem delete  <store> <id>     forget one memory
  mem history <store> <id>     everything that has happened to one memory

options:
  --user <id>         whose memories these are
  --agent <id>        which agent they belong to
  --run <id>          which run of that agent
  --metadata <json>   a JSON object to attach to what is inserted
  --limit <n>         how many to answer with (search, list)
  --json              answer as JSON rather than as lines
  --                  everything after this is text, not options
  -h, --help          this

<store> is a path in this tree, resolved like any other argument. `insert` creates it;
every other command expects it to be there already.
";

/// How many memories `search` answers with unless told otherwise.
const SEARCH_LIMIT: usize = 10;

/// How many `list` answers with unless told otherwise. Larger, because listing is a question
/// about the store rather than about a query, and the answer nobody wants is a truncated one.
const LIST_LIMIT: usize = 100;

/// The `mem` command, as a name a console can delegate.
///
/// # Why a store is an argument and not a setting
///
/// Every command names its store, because a store is a document: a session works on a tree, and
/// which memories belong to what it is doing is the caller's business, not a path configured
/// once for the process. Two stores side by side in the same directory are the normal case.
///
/// That is also what makes the mount necessary rather than convenient. `notes.sqlite` is a name
/// in the *workspace*, and SQLite opens files by host path — so the name is resolved against
/// where the calling command stood ([`ExecCall::resolve`]) and then through the mount
/// ([`Mount::host_path`]), which is the same file the caller would have seen. With nothing
/// mounted there is no such file and no honest substitute for one, so the command refuses.
///
/// # What it carries
///
/// The two providers, shared: one `mem` is registered on a console and every call goes through
/// it, so they are behind [`Arc`]s and used from whatever task is running. Which embedder is
/// held is not a detail — it is half of the identity of every store this writes, and a store
/// opened under a different one is refused rather than searched wrongly. See [`Embedder`].
pub struct Mem {
    embedder: Arc<dyn Embedder>,
    inference: Arc<dyn Inference>,
}

impl Mem {
    /// `mem` backed by `embedder` and `inference`.
    pub fn new(embedder: Arc<dyn Embedder>, inference: Arc<dyn Inference>) -> Mem {
        Mem {
            embedder,
            inference,
        }
    }

    /// One line for [`ExecutableSet::register`](cortex::executable::ExecutableSet::register).
    pub fn summary() -> &'static str {
        "remember and recall facts, in a store that is one file of this tree"
    }

    /// The whole command, with everything that can go wrong in one place.
    async fn run(&self, call: &ExecCall, mount: Option<&dyn Mount>) -> Result<Vec<u8>, Failure> {
        let args = match Args::parse(&call.args)? {
            Parsed::Help => return Ok(USAGE.into()),
            Parsed::Run(args) => args,
        };

        let Some(mount) = mount else {
            return Err(Failure {
                code: 1,
                message: "mem: nothing is mounted, so there is no store to open".into(),
            });
        };
        let workspace_path = call.resolve(&args.store).map_err(|e| Failure {
            code: 1,
            message: format!("mem: {}: {e}", args.store),
        })?;
        let path = mount.host_path(&workspace_path);

        let embedder = Arc::clone(&self.embedder);
        let inference = Arc::clone(&self.inference);
        let memory = match args.command.access() {
            Access::Create => Memory::create(&path, embedder, inference).await,
            Access::Write => Memory::open(&path, embedder, inference).await,
            Access::Read => Memory::read(&path, embedder, inference).await,
        }
        // The name the caller used, not the one SQLite was given: the host path is on the far
        // side of a mount the caller cannot see, and naming it says nothing about which store
        // was meant.
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => Failure {
                code: 1,
                message: format!("mem: no store at {}", args.store),
            },
            _ => Failure::from(e),
        })?;

        match args.command {
            Command::Insert => {
                let applied = memory
                    .add(&args.rest[0], &args.scope, &args.metadata)
                    .await?;
                Ok(changes(&applied, args.json))
            }

            Command::Search => {
                let hits = memory
                    .search(
                        &args.rest[0],
                        &args.scope,
                        args.limit.unwrap_or(SEARCH_LIMIT),
                    )
                    .await?;
                Ok(if args.json {
                    json(hits.iter().map(hit_json).collect())
                } else {
                    lines(hits.iter().map(|h| {
                        format!(
                            "{:.3}\t{}\t{}",
                            h.score,
                            h.record.id,
                            one_line(&h.record.memory)
                        )
                    }))
                })
            }

            Command::List => {
                let records = memory
                    .all(&args.scope, args.limit.unwrap_or(LIST_LIMIT))
                    .await?;
                Ok(memories(&records, args.json))
            }

            Command::Get => {
                let found = memory.get(&args.rest[0]).await?.ok_or_else(|| Failure {
                    code: 1,
                    message: format!("mem: no memory {}", args.rest[0]),
                })?;
                Ok(memories(std::slice::from_ref(&found), args.json))
            }

            Command::Delete => {
                let applied = memory.delete(&args.rest[0]).await?;
                if applied.is_empty() {
                    return Err(Failure {
                        code: 1,
                        message: format!("mem: no memory {}", args.rest[0]),
                    });
                }
                Ok(changes(&applied, args.json))
            }

            Command::History => {
                let entries = memory.history(&args.rest[0]).await?;
                Ok(if args.json {
                    json(entries.iter().map(entry_json).collect())
                } else {
                    lines(entries.iter().map(|e| {
                        format!(
                            "{}\t{}\t{}",
                            e.at,
                            e.event,
                            one_line(e.after.as_deref().or(e.before.as_deref()).unwrap_or(""))
                        )
                    }))
                })
            }
        }
    }
}

/// Delegated `mem`, answering on stdout and stderr like the program it stands in for.
impl Executable for Mem {
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecResult> {
        Box::pin(async move {
            match self.run(call, mount).await {
                Ok(stdout) => ExecResult::ok(stdout),
                // Newline-terminated like every other line this writes: a reason that runs into
                // the next thing on the terminal reads as part of it.
                Err(failure) => {
                    ExecResult::failed(failure.code, format!("{}\n", failure.message.trim_end()))
                }
            }
        })
    }
}

/// A command that did not happen, and what to exit with.
///
/// Two codes, because a caller can act on the difference: `2` is a command that was not
/// understood and can be reissued differently, `1` is one that was understood and did not
/// work. Both put their reason on stderr and nothing on stdout, so a caller reading output
/// never reads an explanation as data.
struct Failure {
    code: i32,
    message: String,
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Failure {
        Failure {
            code: 1,
            message: format!("mem: {e}"),
        }
    }
}

/// A misuse, answered with the reason and the usage under it.
fn misuse(what: impl std::fmt::Display) -> Failure {
    Failure {
        code: 2,
        message: format!("mem: {what}\n\n{USAGE}"),
    }
}

/// What a command needs of the file it names.
///
/// The distinction the store draws, decided per command rather than per call: only `insert`
/// brings a store into being, only `insert` and `delete` write to one, and everything else
/// opens a file it will not modify. A `search` that created an empty store, or a `list` that
/// took a write lock on a mount that has none to give, would each be doing something nobody
/// asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Create,
    Write,
    Read,
}

/// Which of the six.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Insert,
    Search,
    List,
    Get,
    Delete,
    History,
}

impl Command {
    /// The name, and how many positionals follow the store.
    fn parse(word: &str) -> Option<(Command, usize)> {
        Some(match word {
            // `add` as well as `insert`: the same operation is spelled the first way by mem0's
            // own API, and a caller that knows one should not have to learn the other.
            "insert" | "add" => (Command::Insert, 1),
            "search" => (Command::Search, 1),
            "list" => (Command::List, 0),
            "get" => (Command::Get, 1),
            "delete" => (Command::Delete, 1),
            "history" => (Command::History, 1),
            _ => return None,
        })
    }

    /// What this needs of the store it names.
    fn access(self) -> Access {
        match self {
            Command::Insert => Access::Create,
            Command::Delete => Access::Write,
            Command::Search | Command::List | Command::Get | Command::History => Access::Read,
        }
    }
}

/// What a command line asked for.
enum Parsed {
    /// `--help` anywhere, which is a request in its own right rather than a flag on a command:
    /// somebody who has to ask does not also have to get the rest of the line right.
    Help,
    Run(Args),
}

/// A parsed command line.
struct Args {
    command: Command,
    store: String,
    /// The positionals after the store: a text, a query, an id, or nothing.
    rest: Vec<String>,
    scope: Scope,
    metadata: String,
    limit: Option<usize>,
    json: bool,
}

impl Args {
    /// Parse `args` — everything after the name the command was invoked by.
    ///
    /// Options may appear anywhere, including after the positionals, because that is where a
    /// person writing a long `insert` puts them. `--` ends them, which is how a text that
    /// starts with a dash is passed at all.
    fn parse(args: &[String]) -> Result<Parsed, Failure> {
        let mut positional: Vec<String> = Vec::new();
        let mut scope = Scope::default();
        let mut metadata = String::from("{}");
        let mut limit = None;
        let mut json = false;
        let mut help = false;
        let mut options = true;

        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut value = |flag: &str| -> Result<String, Failure> {
                it.next()
                    .cloned()
                    .ok_or_else(|| misuse(format!("{flag} wants a value")))
            };
            match arg.as_str() {
                "--" if options => options = false,
                "-h" | "--help" if options => help = true,
                "--user" if options => scope.user = value("--user")?,
                "--agent" if options => scope.agent = value("--agent")?,
                "--run" if options => scope.run = value("--run")?,
                "--metadata" if options => metadata = value("--metadata")?,
                "--json" if options => json = true,
                "--limit" if options => {
                    let n = value("--limit")?;
                    limit = Some(
                        n.parse()
                            .map_err(|_| misuse(format!("--limit wants a number, not {n}")))?,
                    );
                }
                other if options && other.starts_with('-') && other.len() > 1 => {
                    return Err(misuse(format!("no such option: {other}")));
                }
                other => positional.push(other.to_string()),
            }
        }

        // Answered before anything else is required, so `mem --help` and `mem insert --help`
        // both say what the options are rather than complaining about what is missing.
        if help {
            return Ok(Parsed::Help);
        }

        let Some(word) = positional.first() else {
            return Err(misuse("no command"));
        };
        let Some((command, wanted)) = Command::parse(word) else {
            return Err(misuse(format!("no such command: {word}")));
        };

        let rest: Vec<String> = positional[1..].to_vec();
        let Some((store, rest)) = rest.split_first() else {
            return Err(misuse(format!("{word} wants a store to work on")));
        };
        if rest.len() != wanted {
            return Err(misuse(format!(
                "{word} wants {wanted} argument(s) after the store, and was given {}",
                rest.len()
            )));
        }

        if metadata != "{}"
            && !serde_json::from_str::<serde_json::Value>(&metadata).is_ok_and(|v| v.is_object())
        {
            return Err(misuse("--metadata wants a JSON object"));
        }

        Ok(Parsed::Run(Args {
            command,
            store: store.clone(),
            rest: rest.to_vec(),
            scope,
            metadata,
            limit,
            json,
        }))
    }
}

/// What was written to the store: one line each, `event`, id and memory.
///
/// `insert` and `delete` answer in the same shape because they are the same answer — a list of
/// changes — and a caller that reads one should not have to learn a second format for the
/// other.
fn changes(applied: &[Applied], as_json: bool) -> Vec<u8> {
    if as_json {
        json(applied.iter().map(applied_json).collect())
    } else {
        lines(
            applied
                .iter()
                .map(|a| format!("{}\t{}\t{}", a.event, a.id, one_line(&a.memory))),
        )
    }
}

/// Memories the store holds: one line each, id and memory.
fn memories(records: &[Record], as_json: bool) -> Vec<u8> {
    if as_json {
        json(records.iter().map(record_json).collect())
    } else {
        lines(
            records
                .iter()
                .map(|r| format!("{}\t{}", r.id, one_line(&r.memory))),
        )
    }
}

/// Lines on stdout, each ending in a newline. No lines is no bytes, which is what a caller
/// counting results reads as none.
fn lines(lines: impl IntoIterator<Item = String>) -> Vec<u8> {
    let mut out = String::new();
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out.into_bytes()
}

/// A memory as one line: whatever it is, it takes up one, because the line format is what a
/// caller splits on.
fn one_line(text: &str) -> String {
    text.replace(['\n', '\r', '\t'], " ")
}

/// A JSON array, one line, newline-terminated.
fn json(values: Vec<serde_json::Value>) -> Vec<u8> {
    let mut out = serde_json::Value::Array(values).to_string();
    out.push('\n');
    out.into_bytes()
}

/// Whatever was attached to a memory, as JSON — and as a string if it somehow is not.
fn metadata_json(metadata: &str) -> serde_json::Value {
    serde_json::from_str(metadata).unwrap_or_else(|_| serde_json::Value::String(metadata.into()))
}

fn record_json(record: &Record) -> serde_json::Value {
    serde_json::json!({
        "id": record.id,
        "memory": record.memory,
        "user_id": record.scope.user,
        "agent_id": record.scope.agent,
        "run_id": record.scope.run,
        "metadata": metadata_json(&record.metadata),
        "created_at": record.created_at,
        "updated_at": record.updated_at,
    })
}

fn hit_json(hit: &Hit) -> serde_json::Value {
    let mut value = record_json(&hit.record);
    value["score"] = serde_json::json!(hit.score);
    value
}

fn applied_json(applied: &Applied) -> serde_json::Value {
    serde_json::json!({
        "id": applied.id,
        "event": applied.event.as_str(),
        "memory": applied.memory,
    })
}

fn entry_json(entry: &Entry) -> serde_json::Value {
    serde_json::json!({
        "id": entry.id,
        "event": entry.event.as_str(),
        "old_memory": entry.before,
        "new_memory": entry.after,
        "created_at": entry.at,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::{HashEmbedder, Verbatim};

    /// A directory standing in for a mounted tree. Putting a real mount up needs a binding, a
    /// libfuse provider and a kernel; what is under test here is what `mem` does with the path
    /// a mount gives it, and a plain directory answers one the same way.
    struct Mounted(PathBuf);

    impl Mount for Mounted {
        fn mountpoint(&self) -> &Path {
            &self.0
        }
    }

    fn mem() -> Mem {
        Mem::new(Arc::new(HashEmbedder::default()), Arc::new(Verbatim))
    }

    fn call(args: &[&str]) -> ExecCall {
        ExecCall {
            name: "mem".into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            // At the root of the tree, which is where a relative store name resolves from.
            cwd: Some(String::new()),
        }
    }

    async fn run(mount: &Mounted, args: &[&str]) -> ExecResult {
        mem().exec(&call(args), Some(mount)).await
    }

    /// stdout as text, having asserted the command succeeded.
    async fn out(mount: &Mounted, args: &[&str]) -> String {
        let result = run(mount, args).await;
        assert_eq!(
            result.exit_code,
            0,
            "{args:?} failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).expect("mem writes text")
    }

    fn mounted() -> (tempfile::TempDir, Mounted) {
        let dir = tempfile::tempdir().unwrap();
        let mount = Mounted(dir.path().to_path_buf());
        (dir, mount)
    }

    #[tokio::test]
    async fn what_was_inserted_is_what_search_finds() {
        let (_dir, mount) = mounted();
        let inserted = out(&mount, &["insert", "m.sqlite", "the user drinks tea"]).await;
        assert!(inserted.starts_with("ADD\t"), "{inserted:?}");

        let found = out(&mount, &["search", "m.sqlite", "what does the user drink"]).await;
        assert!(found.contains("the user drinks tea"), "{found:?}");
    }

    /// The chain the mount exists for: a name in the workspace, resolved against where the
    /// caller stood, is a file on this host — and the store lands there and nowhere else.
    #[tokio::test]
    async fn the_store_is_a_file_where_the_tree_says_it_is() {
        let (dir, mount) = mounted();
        std::fs::create_dir(dir.path().join("notes")).unwrap();
        let call = ExecCall {
            name: "mem".into(),
            args: ["insert", "m.sqlite", "the user drinks tea"]
                .iter()
                .map(|a| a.to_string())
                .collect(),
            cwd: Some("notes".into()),
        };

        let result = mem().exec(&call, Some(&mount)).await;
        assert_eq!(result.exit_code, 0);
        assert!(dir.path().join("notes/m.sqlite").exists());
        assert!(!dir.path().join("m.sqlite").exists());
    }

    /// Nothing mounted is no file to open and no honest substitute for one.
    #[tokio::test]
    async fn with_nothing_mounted_the_command_refuses() {
        let result = mem()
            .exec(&call(&["insert", "m.sqlite", "hello"]), None)
            .await;
        assert_eq!(result.exit_code, 1);
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("nothing is mounted"),
            "{:?}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.stdout.is_empty());
    }

    /// Reading is not a reason to create a store, and the answer says which file was missing
    /// rather than pretending there were no memories in it.
    ///
    /// Named as the caller named it: the host path is on the far side of a mount they cannot
    /// see, and would not tell them which store was meant.
    #[tokio::test]
    async fn searching_a_store_that_is_not_there_fails_without_making_one() {
        let (dir, mount) = mounted();
        let result = run(&mount, &["search", "absent.sqlite", "tea"]).await;
        assert_eq!(result.exit_code, 1);
        assert!(!dir.path().join("absent.sqlite").exists());

        let said = String::from_utf8(result.stderr).unwrap();
        assert_eq!(said, "mem: no store at absent.sqlite\n");
    }

    /// Every reason ends its line, whatever produced it — one that ran into the next thing on
    /// the terminal would read as part of it.
    #[tokio::test]
    async fn a_reason_is_one_terminated_line() {
        let (_dir, mount) = mounted();
        for args in [
            vec!["search", "absent.sqlite", "tea"],
            vec!["insert", "../outside.sqlite", "tea"],
            vec!["nonsense"],
        ] {
            let stderr = String::from_utf8(run(&mount, &args).await.stderr).unwrap();
            assert!(
                stderr.ends_with('\n') && !stderr.ends_with("\n\n"),
                "{stderr:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_scope_is_what_a_search_is_confined_to() {
        let (_dir, mount) = mounted();
        out(
            &mount,
            &["insert", "m.sqlite", "drinks tea", "--user", "ana"],
        )
        .await;
        out(
            &mount,
            &["insert", "m.sqlite", "drinks coffee", "--user", "bo"],
        )
        .await;

        let ana = out(&mount, &["search", "m.sqlite", "drinks", "--user", "ana"]).await;
        assert!(ana.contains("drinks tea"), "{ana:?}");
        assert!(!ana.contains("drinks coffee"), "{ana:?}");

        let everyone = out(&mount, &["search", "m.sqlite", "drinks"]).await;
        assert!(everyone.contains("drinks tea") && everyone.contains("drinks coffee"));
    }

    #[tokio::test]
    async fn a_memory_can_be_listed_read_forgotten_and_accounted_for() {
        let (_dir, mount) = mounted();
        let inserted = out(&mount, &["insert", "m.sqlite", "the user drinks tea"]).await;
        let id = inserted.split('\t').nth(1).unwrap().to_string();

        let listed = out(&mount, &["list", "m.sqlite"]).await;
        assert_eq!(listed, format!("{id}\tthe user drinks tea\n"));

        let got = out(&mount, &["get", "m.sqlite", &id]).await;
        assert!(got.contains("the user drinks tea"));

        let forgotten = out(&mount, &["delete", "m.sqlite", &id]).await;
        assert!(forgotten.starts_with("DELETE\t"), "{forgotten:?}");
        assert_eq!(out(&mount, &["list", "m.sqlite"]).await, "");

        let history = out(&mount, &["history", "m.sqlite", &id]).await;
        assert_eq!(history.lines().count(), 2);
        assert!(history.contains("\tADD\t") && history.contains("\tDELETE\t"));
    }

    /// A limit is a count of answers, and the nearest are the ones kept.
    #[tokio::test]
    async fn a_limit_bounds_what_comes_back() {
        let (_dir, mount) = mounted();
        for text in ["drinks tea", "drinks coffee", "drinks water"] {
            out(&mount, &["insert", "m.sqlite", text]).await;
        }
        let found = out(&mount, &["search", "m.sqlite", "drinks", "--limit", "2"]).await;
        assert_eq!(found.lines().count(), 2);
    }

    #[tokio::test]
    async fn json_answers_carry_what_the_lines_leave_out() {
        let (_dir, mount) = mounted();
        out(
            &mount,
            &[
                "insert",
                "m.sqlite",
                "drinks tea",
                "--user",
                "ana",
                "--metadata",
                r#"{"source":"chat"}"#,
            ],
        )
        .await;

        let found = out(&mount, &["search", "m.sqlite", "tea", "--json"]).await;
        let value: serde_json::Value = serde_json::from_str(&found).expect("--json answers JSON");
        assert_eq!(value[0]["memory"], "drinks tea");
        assert_eq!(value[0]["user_id"], "ana");
        assert_eq!(value[0]["metadata"]["source"], "chat");
        assert!(value[0]["score"].is_number());
    }

    /// A memory written across several lines is still one line of output — the format is what
    /// a caller splits on, so nothing in a memory may look like a record boundary.
    #[tokio::test]
    async fn a_multi_line_memory_stays_on_one_line() {
        let (_dir, mount) = mounted();
        out(&mount, &["insert", "m.sqlite", "drinks tea\nand coffee"]).await;
        let listed = out(&mount, &["list", "m.sqlite"]).await;
        assert_eq!(listed.lines().count(), 1);
        assert!(listed.contains("drinks tea and coffee"));
    }

    /// Usage errors exit `2`, distinct from a command that was understood and failed.
    #[tokio::test]
    async fn a_command_that_makes_no_sense_exits_two_with_the_usage() {
        let (_dir, mount) = mounted();
        for args in [
            vec!["remember", "m.sqlite", "tea"],
            vec!["insert"],
            vec!["insert", "m.sqlite"],
            vec!["insert", "m.sqlite", "one", "two"],
            vec!["search", "m.sqlite", "tea", "--limit", "many"],
            vec!["search", "m.sqlite", "tea", "--nope"],
            vec!["insert", "m.sqlite", "tea", "--metadata", "[1,2]"],
            vec![],
        ] {
            let result = run(&mount, &args).await;
            assert_eq!(result.exit_code, 2, "{args:?} did not read as a misuse");
            assert!(String::from_utf8_lossy(&result.stderr).contains("usage:"));
        }
    }

    /// `--` is how a memory that starts with a dash is written at all.
    #[tokio::test]
    async fn everything_after_a_bare_dash_dash_is_text() {
        let (_dir, mount) = mounted();
        let inserted = out(
            &mount,
            &["insert", "m.sqlite", "--", "--user is not a flag here"],
        )
        .await;
        assert!(
            inserted.contains("--user is not a flag here"),
            "{inserted:?}"
        );
    }

    #[tokio::test]
    async fn help_is_answered_on_stdout_and_succeeds() {
        let (_dir, mount) = mounted();
        assert_eq!(out(&mount, &["--help"]).await, USAGE);
        assert_eq!(out(&mount, &["insert", "--help"]).await, USAGE);
    }
}
