use futures_core::future::BoxFuture;

use crate::fs::Mount;

/// What a delegated executable was asked to do.
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
}

impl ExecCall {
    /// `arg` as a workspace path, resolved against [`cwd`](Self::cwd).
    ///
    /// A leading `/` makes `arg` workspace-absolute and `cwd` irrelevant. Anything that
    /// would leave the root is refused, as is a relative `arg` with no `cwd` — see
    /// [`resolve_under`](crate::executable::resolve_under).
    ///
    /// Relative to the workspace root and never to this host, because the two ends of a
    /// delegated call agree on the first and not the second. What turns it into a file to
    /// open is the mount the call was handed: [`Mount::host_path`].
    pub fn resolve(&self, arg: &str) -> std::io::Result<std::path::PathBuf> {
        crate::executable::resolve_under(self.cwd.as_deref(), arg)
    }
}

/// What a delegated executable produced.
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

/// Something a shell can call that is not a program on disk.
///
/// Implementations live wherever the client does — the whole point is that a
/// console server can put the name on a `PATH` it controls without knowing, or
/// being able to know, what running it means.
///
/// # Why this waits, and why the future is boxed
///
/// A name is worth delegating when what it does is not something the server could
/// have done itself: reach a service, ask a model, look something up. All of that is
/// waiting, and an implementation that blocked while it waited would block the task
/// walking the delegation chain — which is the same task the console channel is
/// answered on.
///
/// [`BoxFuture`] rather than an `async fn` because an [`ExecutableSet`] holds these
/// behind a `dyn`: a name is looked up at run time, so the type behind it cannot be in
/// anyone's signature. The allocation is one per delegated call, next to a round trip
/// out to the server and back.
///
/// # Why the mount is an argument
///
/// A delegated name is called from inside an execution, and the files it is asked about are
/// the ones that execution can see. Those are reachable because the session's tree is
/// *mounted*: the command that invoked this name opened them by path, and a delegated name
/// that is asked about the same file has to be able to open the same path.
///
/// So what arrives is a [`Mount`] and not a store. A store would describe the same tree and
/// name it differently — no host path, nothing a `std::fs` call or a spawned program could
/// use — where a mount is the arrangement both ends of the call already share:
/// [`ExecCall::resolve`] gives the workspace-relative path, [`Mount::host_path`] turns it
/// into the file, and it is the same file the command meant.
///
/// It cannot be captured instead, either. The mount belongs to the console — one per session
/// — and the same `Executable` may be registered on several, so each call belongs to
/// whichever console is asking.
///
/// A shared borrow, because taking the mount down is not a delegated name's to do: dropping
/// it unmounts (see [`Mount`]), which would take the tree out from under the execution still
/// running against it.
///
/// `None` is a console with nothing mounted. A name that touches no files ignores it; one
/// that needs a file has no way to reach one and should say so, which is honest where a
/// substituted path would read something nobody asked about.
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
#[path = "executable_tests.rs"]
mod tests;
