use futures_core::future::BoxFuture;

use crate::volume::Workspace;

/// What a delegated executable was asked to do.
#[derive(Clone, Debug)]
pub struct ExecCall {
    /// The name it was invoked by. The same executable can be registered under
    /// more than one, and a busybox-style implementation branches on it.
    pub name: String,

    /// Everything after the name.
    pub args: Vec<String>,
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
/// # Why the workspace is an argument
///
/// A delegated name is called from inside an execution, and the files it is asked
/// about are the ones that execution can see. That namespace is the console's — it
/// is what the console projects into the server, one per session — so an
/// implementation cannot have captured the right one: the same `Executable` may be
/// registered on several consoles, and each call belongs to whichever one is asking.
///
/// A shared borrow, because mounting is not a delegated name's to do. Everything a
/// name does *to files* — stat, list, open, read, write, rename — is a
/// [`Mountable`](crate::volume::Mountable) operation on `&self`, while
/// [`mount`](Workspace::mount) needs `&mut self` and would rewrite the namespace an
/// execution is already running against.
///
/// [`ExecutableSet`]: super::ExecutableSet
pub trait Executable: Send + Sync {
    fn exec<'a>(
        &'a self,
        call: &'a ExecCall,
        workspace: &'a Workspace,
    ) -> BoxFuture<'a, ExecResult>;
}
