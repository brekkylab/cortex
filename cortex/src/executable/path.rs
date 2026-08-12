//! The path rules a delegated call needs, in one place.
//!
//! A delegated executable runs in the client's process; the command that invoked it ran in
//! the server's. A relative argument means something only if both ends agree on a root, so
//! one function turns an executor-side directory into a workspace-relative one and the other
//! resolves an argument against it.
//!
//! Here rather than in each backend because a backend's `delegate` is written twice, and the
//! `Some`-vs-`None` rule is what must not drift between the copies.

use std::path::{Component, Path, PathBuf};

use crate::{CortexError, Result};

/// `cwd` expressed relative to `root`, or `None` if it cannot be — because it lies outside
/// `root`, or because its name is not UTF-8. `to_string_lossy` there would hand back a path
/// that looks valid and names something else.
///
/// The root itself is `Some("")`, a workspace's own spelling for its root.
///
/// `strip_prefix` is component-wise, which is the point: `/mnt-other` is not under `/mnt`.
pub fn relativize(cwd: &Path, root: &Path) -> Option<String> {
    let rest = cwd.strip_prefix(root).ok()?;
    rest.to_str().map(str::to_owned)
}

/// A shim's own working directory, as the client should be told it.
///
/// A shim reports an executor-side absolute path, which is not a name the client's tree has;
/// only the backend knows what root to take it relative to.
///
/// `None` covers three things on purpose — nothing reported, no namespace, a directory
/// outside it — because all three mean the same to a client, and [`resolve_under`] refuses a
/// relative argument that has no directory. Guessing would be worse than refusing.
pub fn reported_cwd(shim: Option<&str>, root: Option<&Path>) -> Option<String> {
    relativize(Path::new(shim?), root?)
}

/// `arg` resolved against `cwd`, as a root-relative path with no `.` or `..` left in it.
///
/// A leading `/` makes `arg` workspace-absolute, so `cwd` does not apply — which is how a
/// caller names a file without depending on where it stood.
///
/// Errors when a relative `arg` has no `cwd`
/// ([`InvalidArgument`](CortexError::InvalidArgument)), and when the result would leave the
/// root ([`InvalidName`](CortexError::InvalidName)).
///
/// # `..` is resolved only where that is not a guess
///
/// This walk is lexical; a kernel's is not. `nope/../notes.txt` cancels out on paper and
/// fails at `nope` in a real tree, so popping it here is how one name comes to mean two files
/// — the command could not open it and the executable it invoked can.
///
/// So a `..` is resolved when it pops a component of `cwd`, whose every component existed
/// because the command *stood* there, and refused when it pops a name `arg` introduced, which
/// nothing has looked up. `("work/sub", "../x.txt")` is `work/x.txt`;
/// `("work/sub", "nope/../x.txt")` is `InvalidName`. The two ends still disagree there, by
/// refusing rather than by answering.
pub fn resolve_under(cwd: Option<&str>, arg: &str) -> Result<PathBuf> {
    let mut out: Vec<&std::ffi::OsStr> = Vec::new();

    // Entries in `out` that `cwd` put there, and so the pops that are looking at a directory
    // somebody has already been in.
    let mut standing = 0;

    if !arg.starts_with('/') {
        let cwd = Path::new(cwd.ok_or(CortexError::InvalidArgument)?);
        for component in cwd.components() {
            match component {
                Component::Normal(name) => out.push(name),
                Component::CurDir => {}
                // Every producer hands over `relativize`'s output, which has none of these.
                // Refused rather than trusted because the walk would silently reinterpret an
                // absolute `cwd` as relative: `("/etc", "passwd")` would answer `etc/passwd`,
                // a real place nobody asked for.
                Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                    return Err(CortexError::InvalidArgument);
                }
            }
        }
        standing = out.len();
    }

    for component in Path::new(arg).components() {
        match component {
            // A root is where a workspace-absolute `arg` starts, not a name in it.
            Component::RootDir | Component::CurDir => {}
            // A drive letter or UNC share names a volume no workspace contains, and
            // `Workspace::normalize` refuses one too.
            Component::Prefix(_) => return Err(CortexError::InvalidName),
            Component::ParentDir => {
                // Above `standing` is a name `arg` pushed; at zero the path has left the root.
                if out.len() > standing || out.is_empty() {
                    return Err(CortexError::InvalidName);
                }
                out.pop();
                standing -= 1;
            }
            Component::Normal(name) => out.push(name),
        }
    }
    Ok(out.iter().collect())
}

#[cfg(test)]
#[path = "path_tests.rs"]
mod tests;
