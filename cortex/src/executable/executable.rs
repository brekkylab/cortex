/// What a delegated executable was asked to do.
///
/// Nothing about *where* it was asked from. The caller's working directory is what
/// would make a relative path in `args` mean anything — the executable runs in a
/// process that has a different one — and the wire does not carry it yet, so this
/// does not pretend to either.
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
pub trait Executable: Send + Sync {
    fn exec(&self, call: &ExecCall) -> ExecResult;
}
