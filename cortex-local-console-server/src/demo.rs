//! The virtual executables this PoC boots with.

use cortex::executable::{ExecResult, Executable, ExecutableSet};

/// The set this server offers.
pub fn set() -> ExecutableSet {
    ExecutableSet::new().register("foo", Foo)
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
