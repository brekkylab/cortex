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
#[path = "executable_set_tests.rs"]
mod tests;
