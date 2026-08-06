use std::collections::BTreeMap;

use crate::executable::{ExecCall, ExecResult, Executable};

/// A named set of [`Executable`]s — the allowlist one console server offers.
///
/// Name -> behaviour, and nothing about how the name is reached. Surfacing the
/// set is the server's business: the host-local one links each name into a
/// directory it puts on `PATH`, a micro-VM one projects them into the guest.
/// Either way the behaviour stays here, in-process, and [`invoke`] is where a
/// call lands once the server has worked out which name was asked for.
///
/// [`invoke`]: Self::invoke
#[derive(Default)]
pub struct ExecutableSet {
    execs: BTreeMap<String, Box<dyn Executable>>,
}

impl ExecutableSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `exec` under `name` (builder-style; last write wins).
    pub fn register(mut self, name: &str, exec: impl Executable + 'static) -> Self {
        self.execs.insert(name.to_string(), Box::new(exec));
        self
    }

    /// The registered names, sorted — the exact set a server should expose.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.execs.keys().map(String::as_str)
    }

    /// Run the executable `call` names, or `None` if nothing is registered under
    /// it.
    ///
    /// `None` is the allowlist boundary. A server that only ever exposes the
    /// names in this set should not reach it — but the name usually arrives
    /// from outside the process, so it is input, not a guarantee.
    ///
    /// Whether the name was registered is settled before anything is awaited, and the
    /// lookup itself is a map read — so a caller that only wants to know whether a name
    /// is one of ours pays nothing for the waiting it did not ask for.
    pub async fn invoke(&self, call: &ExecCall) -> Option<ExecResult> {
        let exec = self.execs.get(&call.name)?;
        Some(exec.exec(call).await)
    }
}
