//! The wire between a console-server backend and whoever spawned it.
//!
//! One JSON object per line in each direction, MCP-style. JSON rather than raw
//! bytes because a command's own stdout can contain anything — newlines
//! included — so the reply needs a frame the payload cannot forge.
//!
//! Both directions are internally tagged on `"type"`: the discriminant is a
//! field of the object rather than a wrapper around it, so a message reads as
//! one flat record — `{"type":"exec","cmd":["echo","hi"]}` — and a reader can
//! branch on `type` before caring about the rest.

use serde::{Deserialize, Serialize};

/// What the caller asks the backend to do.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Run a command and report what it did.
    Exec(ExecRequest),
    /// Stop serving and shut down. Answered by the backend exiting rather than
    /// by a response, since there is nothing left to say and the closed stdout
    /// tells the caller more reliably than a message would.
    Quit,
}

/// What the backend says back. Exactly one per request, except `Quit`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// The outcome of an [`Request::Exec`].
    Exec(ExecResponse),
    /// The backend could not act on the request at all — it did not parse, or
    /// it asked for something this backend cannot do.
    ///
    /// Distinct from a command that ran and failed, which is an `Exec` response
    /// carrying a non-zero `code`. Here nothing ran.
    Error { message: String },
}

/// A command to run, already split into argv.
///
/// Splitting is the caller's job: the backend never consults a shell, so
/// quoting and word rules stay wherever the request was composed. A caller that
/// wants shell semantics asks for them outright — `["sh", "-c", "..."]`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExecRequest {
    /// First element is the program, the rest are its arguments.
    pub cmd: Vec<String>,
}

/// What the command did.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExecResponse {
    /// Captured in full, decoded lossily — the field is a `String`, so output
    /// that is not valid UTF-8 arrives with replacement characters.
    pub stdout: String,
    /// Captured separately from `stdout`, so the two never interleave here even
    /// though the command may have produced them interleaved.
    pub stderr: String,
    /// The command's exit code, or `128 + signal` when a signal killed it — the
    /// shell's convention, and the only way to fit a signal death into an
    /// integer that otherwise means "exit code".
    pub code: i32,
}
