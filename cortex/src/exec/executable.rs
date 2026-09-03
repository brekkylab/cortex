use futures_core::future::BoxFuture;

use crate::fs::Mount;

/// What an [`Executable`] was asked to do: one command line, and where it was typed.
#[derive(Clone, Debug)]
pub struct ExecCall {
    /// The name it was invoked by. The same executable can be registered under
    /// more than one, and a busybox-style implementation branches on it.
    pub name: String,

    /// Everything after the name.
    pub args: Vec<String>,

    /// Where the command that invoked this stood, **relative to the workspace root** — or
    /// `None` when there was no workspace, when it stood outside one, or when the
    /// directory's name has no `String` form.
    ///
    /// An executable that names no paths can ignore it. One that does should reach for
    /// [`resolve`](Self::resolve) rather than joining by hand: `None` there is a refusal,
    /// which is the honest answer, where a substituted root would read a different file and
    /// say nothing about it.
    pub cwd: Option<String>,

    /// The environment the invoking command had — all of it, as the executor reported it.
    ///
    /// What makes one of these behave like a program rather than almost like one:
    /// `FOO=bar summarize x` reaches here as `FOO`, and so does anything exported earlier in
    /// the same shell.
    ///
    /// **Read, never applied.** This is what the *caller* had, not what this process has,
    /// and the two need not be the same machine's worth of facts — `PWD` and `HOME` are the
    /// caller's. An executable that wants a variable looks it up here; one that sets it
    /// anywhere has changed the wrong process.
    ///
    /// Empty when there was nothing to report and when the command genuinely had no
    /// environment, which are the same thing to a lookup.
    pub env: std::collections::BTreeMap<String, String>,
}

impl ExecCall {
    /// `arg` as a workspace path, resolved against [`cwd`](Self::cwd).
    ///
    /// A leading `/` makes `arg` workspace-absolute and `cwd` irrelevant. Anything that
    /// would leave the root is refused, as is a relative `arg` with no `cwd` — see
    /// [`resolve_under`](crate::exec::resolve_under).
    ///
    /// Relative to the workspace root and never to this host, because a workspace path is
    /// the one spelling that means the same file to whoever composed the command and to
    /// whatever answers it. What turns it into a file to open is the mount the call was
    /// handed: [`Mount::host_path`].
    pub fn resolve(&self, arg: &str) -> std::io::Result<std::path::PathBuf> {
        crate::exec::resolve_under(self.cwd.as_deref(), arg)
    }
}

/// What an [`Executable`] produced.
///
/// `Vec<u8>`, not `String`: this is program output on its way to a caller's pipe,
/// and a caller that feeds it to something byte-oriented has to get back exactly
/// what was written.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub timed_out: bool,
}

impl ExecResult {
    /// Succeeded, with `stdout` and nothing on `stderr`.
    pub fn ok(stdout: impl Into<Vec<u8>>) -> Self {
        ExecResult {
            stdout: stdout.into(),
            stderr: Vec::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    /// Failed with `exit_code`, the reason on `stderr`.
    pub fn failed(exit_code: i32, stderr: impl Into<Vec<u8>>) -> Self {
        ExecResult {
            stdout: Vec::new(),
            stderr: stderr.into(),
            exit_code,
            timed_out: false,
        }
    }
}

/// A command implemented in this process: an argv in, output and a code out.
///
/// The same shape a program on disk has, minus the disk — which is the whole point.
/// Something that reaches a service, asks a model or looks a fact up is a few lines of
/// Rust and no lines of packaging, and what runs it is handed the same three things a
/// program is: the name it was invoked by, its arguments, and the tree its path arguments
/// are about.
///
/// # Why this waits, and why the future is boxed
///
/// What one of these is worth writing for is precisely the work a synchronous function
/// would be wrong for: a request, an inference, a query. So the method is something to
/// `await`, and an implementation that blocks while it waits blocks whatever is driving it.
///
/// [`BoxFuture`] rather than an `async fn` because an [`ExecutableSet`] holds these behind
/// a `dyn`: a name is looked up at run time, so the type behind it cannot be in anyone's
/// signature. The allocation is one per call, next to whatever the call is actually going
/// off to do.
///
/// # Why the mount is an argument
///
/// The files one of these is asked about are the ones the caller could see, and the caller
/// could see them because the tree is *mounted*: it opened them by path, and a name asked
/// about the same file has to be able to open the same path.
///
/// So what arrives is a [`Mount`] and not a store. A store would describe the same tree and
/// name it differently — no host path, nothing a `std::fs` call or a spawned program could
/// use — where a mount is the arrangement both sides already share: [`ExecCall::resolve`]
/// gives the workspace-relative path, [`Mount::host_path`] turns it into the file, and it is
/// the same file the caller meant.
///
/// It cannot be captured instead, either. The same `Executable` may be registered against
/// several trees, so which one a call is about belongs to the call.
///
/// A shared borrow, because taking the mount down is not an executable's to do: dropping it
/// unmounts (see [`Mount`]), which would take the tree out from under whatever else is
/// running against it.
///
/// `None` is nothing mounted. A name that touches no files ignores it; one that needs a
/// file has no way to reach one and should say so, which is honest where a substituted path
/// would read something nobody asked about.
///
/// [`ExecutableSet`]: super::ExecutableSet
pub trait Executable: Send + Sync {
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        mount: Option<&'a dyn Mount>,
    ) -> BoxFuture<'a, ExecResult>;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn call(cwd: Option<&str>, args: &[&str]) -> ExecCall {
        ExecCall {
            name: "summarize".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.map(str::to_owned),
            env: Default::default(),
        }
    }

    #[test]
    fn resolve_uses_the_calls_own_directory() {
        let c = call(Some("docs"), &["report.md"]);
        assert_eq!(
            c.resolve(&c.args[0]).unwrap(),
            PathBuf::from("docs/report.md")
        );
    }

    #[test]
    fn resolve_refuses_to_leave_the_workspace() {
        let c = call(Some("docs"), &["../../etc/passwd"]);
        assert!(c.resolve(&c.args[0]).is_err());
    }

    /// The honest answer when nothing said where the caller stood — a substituted root would
    /// read a different file and say nothing about it.
    #[test]
    fn resolve_refuses_a_relative_argument_when_the_directory_is_unknown() {
        let c = call(None, &["report.md"]);
        assert!(c.resolve(&c.args[0]).is_err());
    }

    /// An executable that names files absolutely needs no directory at all.
    #[test]
    fn resolve_takes_an_absolute_argument_without_a_directory() {
        let c = call(None, &["/docs/report.md"]);
        assert_eq!(
            c.resolve(&c.args[0]).unwrap(),
            PathBuf::from("docs/report.md")
        );
    }
}
