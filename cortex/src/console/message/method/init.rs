use serde::{Deserialize, Serialize};

/// What a session is. The `params` of `init`.
///
/// What is here outlives any one execution, which is what it is doing here rather than on
/// an [`Exec`](super::Exec): the delegated names have to be in place before a command that
/// invokes one runs, so they are said once instead of on every command.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Init {
    /// Empty is not an error — a client with nothing to delegate is still a client.
    ///
    /// Meant to be sorted and free of duplicates, and **nothing checks either**. A client
    /// that repeats a name gets a session that will not boot: a backend makes one entry per
    /// name and the second collides, which arrives as `BOOT_FAILED` describing a symlink
    /// rather than the name that was said twice. It costs that client its own session and
    /// nobody else's, which is why this is written down rather than enforced — but a
    /// backend that wants to say something useful about it should check before it builds.
    ///
    /// What running one *means* is not here and cannot be: the behaviour lives in
    /// the client's [`ExecutableSet`](crate::executable::ExecutableSet), so a
    /// server only arranges for something that runs the name to reach the client —
    /// on a channel of its own, as an `exec` like any other.
    pub delegated: Vec<String>,
}
