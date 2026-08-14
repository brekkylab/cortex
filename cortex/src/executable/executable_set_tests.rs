use futures_core::future::BoxFuture;

use super::*;

struct Echo;

impl Executable for Echo {
    fn exec<'a>(&'a self, call: &'a ExecCall, _ws: &'a WorkFs) -> BoxFuture<'a, ExecResult> {
        Box::pin(async move { ExecResult::ok(call.args.join(" ")) })
    }
}

/// A second behaviour, so a test can tell which one a name reached.
struct Shout;

impl Executable for Shout {
    fn exec<'a>(&'a self, call: &'a ExecCall, _ws: &'a WorkFs) -> BoxFuture<'a, ExecResult> {
        Box::pin(async move { ExecResult::ok(call.args.join(" ").to_uppercase()) })
    }
}

fn call(name: &str, args: &[&str]) -> ExecCall {
    ExecCall {
        name: name.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: None,
    }
}

/// Sorted by name whatever order they were registered in — a list an agent reads should not
/// depend on wiring order.
#[test]
fn entries_pair_every_name_with_its_summary() {
    let set = ExecutableSet::new()
        .register("zed", "say it back, last", Echo)
        .register("alpha", "say it back, first", Echo);

    assert_eq!(
        set.entries().collect::<Vec<_>>(),
        [
            ("alpha", "say it back, first"),
            ("zed", "say it back, last")
        ]
    );
    assert_eq!(set.names().collect::<Vec<_>>(), ["alpha", "zed"]);
}

/// One executable under two names carries a summary per name — what a description on the
/// executable could not express.
#[test]
fn the_same_executable_can_be_summarized_differently_per_name() {
    let set = ExecutableSet::new()
        .register("echo", "say it back", Echo)
        .register("repeat", "say it back, again", Echo);

    assert_eq!(
        set.entries().collect::<Vec<_>>(),
        [("echo", "say it back"), ("repeat", "say it back, again")]
    );
}

/// An empty line is a value, not an absence — `register` has no default to fall back to.
#[test]
fn an_empty_summary_is_kept_as_it_was_given() {
    let set = ExecutableSet::new().register("quiet", "", Echo);
    assert_eq!(set.entries().collect::<Vec<_>>(), [("quiet", "")]);
}

/// Last write wins replaces **both** halves, so a name cannot end up with an old behaviour
/// under a new summary.
#[tokio::test]
async fn re_registering_a_name_replaces_the_summary_and_the_behaviour() {
    let set = ExecutableSet::new()
        .register("say", "say it back", Echo)
        .register("say", "say it back, louder", Shout);

    assert_eq!(
        set.entries().collect::<Vec<_>>(),
        [("say", "say it back, louder")]
    );

    let result = set
        .invoke(&call("say", &["hi", "there"]), &WorkFs::new())
        .await
        .expect("registered");
    assert_eq!(result.stdout, b"HI THERE");
}

/// An unregistered name is `None` — the allowlist boundary — and a registered one reaches
/// its own executable, not whichever sorts first.
#[tokio::test]
async fn a_name_reaches_the_executable_it_was_registered_with() {
    let set = ExecutableSet::new()
        .register("echo", "say it back", Echo)
        .register("shout", "say it back, louder", Shout);
    let ws = WorkFs::new();

    assert!(set.invoke(&call("nope", &[]), &ws).await.is_none());

    let quiet = set
        .invoke(&call("echo", &["hi"]), &ws)
        .await
        .expect("registered");
    assert_eq!(quiet.stdout, b"hi");

    let loud = set
        .invoke(&call("shout", &["hi"]), &ws)
        .await
        .expect("registered");
    assert_eq!(loud.stdout, b"HI");
}
