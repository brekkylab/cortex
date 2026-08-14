use std::collections::BTreeMap;

use crate::{
    executable::{ExecCall, ExecResult, Executable},
    fs::Mount,
};

/// A named set of [`Executable`]s — the allowlist one console server offers.
///
/// Name -> behaviour and the line describing it, and nothing about how the name
/// is reached. Surfacing the set is the server's business: the host-local one
/// links each name into a directory it puts on `PATH`, a micro-VM one projects
/// them into the guest. Either way the behaviour stays here, in-process, and
/// [`invoke`] is where a call lands once the server has worked out which name
/// was asked for.
///
/// [`names`](Self::names) is what a server exposes; [`entries`](Self::entries) is
/// what assembles an agent's list.
///
/// [`invoke`]: Self::invoke
#[derive(Default)]
pub struct ExecutableSet {
    execs: BTreeMap<String, (String, Box<dyn Executable>)>,
}

impl ExecutableSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `exec` under `name` (builder-style; last write wins).
    ///
    /// `summary` is one line for a list, not usage — usage is `<name> --help`, which the
    /// executable answers itself. Required and undefaulted, so `""` is a decision rather
    /// than an oversight; here rather than on [`Executable`], so one executable under two
    /// names can describe each.
    pub fn register(
        mut self,
        name: &str,
        summary: impl Into<String>,
        exec: impl Executable + 'static,
    ) -> Self {
        self.execs
            .insert(name.to_string(), (summary.into(), Box::new(exec)));
        self
    }

    /// The registered names, sorted — the exact set a server should expose.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.execs.keys().map(String::as_str)
    }

    /// Every name with its summary, sorted by name — what a consumer renders into whatever
    /// its agent reads.
    ///
    /// Nothing is rendered here: the shape that suits depends on the agent, and a renderer
    /// pointing at `--help` would promise what only the implementor can keep.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.execs
            .iter()
            .map(|(name, (summary, _))| (name.as_str(), summary.as_str()))
    }

    /// Run the executable `call` names against `mount`, or `None` if nothing is
    /// registered under it.
    ///
    /// `mount` is where the tree the execution runs against is mounted, and it is passed
    /// through rather than held: a set is a name table, and the same one is registered on
    /// consoles that mount different trees (see [`Executable::exec`], which also says what
    /// `None` there means).
    ///
    /// `None` is the allowlist boundary. A server that only ever exposes the
    /// names in this set should not reach it — but the name usually arrives
    /// from outside the process, so it is input, not a guarantee.
    ///
    /// Whether the name was registered is settled before anything is awaited, and the
    /// lookup itself is a map read — so a caller that only wants to know whether a name
    /// is one of ours pays nothing for the waiting it did not ask for.
    pub async fn invoke(&self, call: &ExecCall, mount: Option<&dyn Mount>) -> Option<ExecResult> {
        let (_, exec) = self.execs.get(&call.name)?;
        Some(exec.exec(call, mount).await)
    }
}

#[cfg(test)]
mod tests {
    use futures_core::future::BoxFuture;

    use super::*;

    struct Echo;

    impl Executable for Echo {
        fn exec<'a>(
            &'a self,
            call: &'a ExecCall,
            _mount: Option<&'a dyn Mount>,
        ) -> BoxFuture<'a, ExecResult> {
            Box::pin(async move { ExecResult::ok(call.args.join(" ")) })
        }
    }

    /// A second behaviour, so a test can tell which one a name reached.
    struct Shout;

    impl Executable for Shout {
        fn exec<'a>(
            &'a self,
            call: &'a ExecCall,
            _mount: Option<&'a dyn Mount>,
        ) -> BoxFuture<'a, ExecResult> {
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

    /// Sorted by name whatever order they were registered in — a list an agent reads should
    /// not depend on wiring order.
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

    /// Last write wins replaces **both** halves, so a name cannot end up with an old
    /// behaviour under a new summary.
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
            .invoke(&call("say", &["hi", "there"]), None)
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

        assert!(set.invoke(&call("nope", &[]), None).await.is_none());

        let quiet = set
            .invoke(&call("echo", &["hi"]), None)
            .await
            .expect("registered");
        assert_eq!(quiet.stdout, b"hi");

        let loud = set
            .invoke(&call("shout", &["hi"]), None)
            .await
            .expect("registered");
        assert_eq!(loud.stdout, b"HI");
    }
}
