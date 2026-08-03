//! The set of virtual executables this server offers, and the one demo entry
//! the PoC ships with.

use std::collections::BTreeMap;

use cortex::{ExecResult, Executable};

/// Name -> behaviour. The names become symlinks in the
/// [`BinDir`](crate::bin_dir::BinDir); the behaviour runs here, in-process,
/// when a shim reports that one of them was called.
#[derive(Default)]
pub struct Registry {
    execs: BTreeMap<String, Box<dyn Executable>>,
}

impl Registry {
    /// The set this server boots with.
    pub fn demo() -> Self {
        Registry::default().register("foo", Foo)
    }

    /// Add `exec` under `name` (builder-style; last write wins).
    pub fn register(mut self, name: &str, exec: impl Executable + 'static) -> Self {
        self.execs.insert(name.to_string(), Box::new(exec));
        self
    }

    /// The registered names, sorted — the exact set to link into `bin/`.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.execs.keys().map(String::as_str)
    }

    /// Run the named executable, or `None` if nothing is registered under it.
    ///
    /// `None` should be unreachable in practice — a name only becomes callable
    /// by being linked from this same set — but the shim's `name` arrives over a
    /// socket, so it is input, not a guarantee.
    pub fn invoke(&self, name: &str, args: Vec<String>) -> Option<ExecResult> {
        let exec = self.execs.get(name)?;
        Some(exec.exec(name.to_string(), args))
    }
}

/// Prints `bar`. The smallest thing that proves the whole path: a name on
/// `PATH`, resolved by `execvp`, answered by Rust in another process.
struct Foo;

impl Executable for Foo {
    fn exec(&self, _program: String, _args: Vec<String>) -> ExecResult {
        ExecResult {
            stdout: "bar\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }
}
