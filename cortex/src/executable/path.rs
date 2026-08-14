//! The path rules a delegated call needs, in one place.
//!
//! A delegated executable runs in the client's process; the command that invoked it ran in
//! the server's. A relative argument means something only if both ends agree on a root, so
//! one function turns an executor-side directory into a workspace-relative one and the other
//! resolves an argument against it.
//!
//! Here rather than in each backend because a backend's `delegate` is written twice, and the
//! `Some`-vs-`None` rule is what must not drift between the copies.

use std::io;
use std::path::{Component, Path, PathBuf};

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
/// Errors when a relative `arg` has no `cwd` ([`InvalidInput`](io::ErrorKind::InvalidInput)),
/// and when the result would leave the root
/// ([`InvalidFilename`](io::ErrorKind::InvalidFilename)).
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
pub fn resolve_under(cwd: Option<&str>, arg: &str) -> io::Result<PathBuf> {
    let mut out: Vec<&std::ffi::OsStr> = Vec::new();

    // Entries in `out` that `cwd` put there, and so the pops that are looking at a directory
    // somebody has already been in.
    let mut standing = 0;

    if !arg.starts_with('/') {
        let cwd = Path::new(cwd.ok_or(io::Error::from(io::ErrorKind::InvalidInput))?);
        for component in cwd.components() {
            match component {
                Component::Normal(name) => out.push(name),
                Component::CurDir => {}
                // Every producer hands over `relativize`'s output, which has none of these.
                // Refused rather than trusted because the walk would silently reinterpret an
                // absolute `cwd` as relative: `("/etc", "passwd")` would answer `etc/passwd`,
                // a real place nobody asked for.
                Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                    return Err(io::ErrorKind::InvalidInput.into());
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
            // `WorkFs`'s own normalization refuses one too.
            Component::Prefix(_) => return Err(io::ErrorKind::InvalidFilename.into()),
            Component::ParentDir => {
                // Above `standing` is a name `arg` pushed; at zero the path has left the root.
                if out.len() > standing || out.is_empty() {
                    return Err(io::ErrorKind::InvalidFilename.into());
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
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn a_directory_under_the_root_becomes_root_relative() {
        let got = relativize(Path::new("/mnt/work/sub"), Path::new("/mnt"));
        assert_eq!(got.as_deref(), Some("work/sub"));
    }

    #[test]
    fn the_root_itself_is_the_empty_path() {
        assert_eq!(
            relativize(Path::new("/mnt"), Path::new("/mnt")).as_deref(),
            Some("")
        );
    }

    #[test]
    fn a_directory_outside_the_root_is_none() {
        assert_eq!(relativize(Path::new("/etc"), Path::new("/mnt")), None);
    }

    /// A sibling whose name merely starts with the root's — not a child. `strip_prefix` is
    /// component-wise, which is the whole reason it is used rather than a string prefix.
    #[test]
    fn a_prefix_that_is_not_a_path_prefix_is_none() {
        assert_eq!(
            relativize(Path::new("/mnt-other/x"), Path::new("/mnt")),
            None
        );
    }

    /// `to_string_lossy` would hand back a path that looks valid and names something else,
    /// which is the failure `cwd` exists to prevent.
    #[test]
    fn a_non_utf8_directory_is_none_rather_than_lossy() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut bytes = b"/mnt/".to_vec();
        bytes.push(0xff);
        let cwd = PathBuf::from(OsString::from_vec(bytes));
        assert_eq!(relativize(&cwd, Path::new("/mnt")), None);
    }

    #[test]
    fn a_reported_directory_inside_the_namespace_is_relative() {
        assert_eq!(
            reported_cwd(Some("/workspace/work/sub"), Some(Path::new("/workspace"))).as_deref(),
            Some("work/sub")
        );
    }

    /// The root's own name in a workspace, which [`resolve_under`] joins onto correctly. A
    /// command that stood at the root stood somewhere, and `None` would say it stood nowhere.
    #[test]
    fn a_reported_root_is_the_empty_path_rather_than_nothing() {
        assert_eq!(
            reported_cwd(Some("/workspace"), Some(Path::new("/workspace"))).as_deref(),
            Some("")
        );
    }

    /// A path the client's tree has no name for is one it must not be told: there, the same
    /// string would resolve to something else.
    #[test]
    fn a_reported_directory_outside_the_namespace_is_nothing() {
        assert_eq!(
            reported_cwd(Some("/etc"), Some(Path::new("/workspace"))),
            None
        );
    }

    #[test]
    fn a_session_with_no_namespace_reports_nothing() {
        assert_eq!(reported_cwd(Some("/anywhere"), None), None);
    }

    #[test]
    fn a_shim_that_reported_nothing_stays_nothing() {
        assert_eq!(reported_cwd(None, Some(Path::new("/workspace"))), None);
    }

    /// An absolute `cwd` is refused rather than quietly read as a relative one.
    ///
    /// Unreachable through the honest path — `reported_cwd` only ever produces a `strip_prefix`
    /// result — which is exactly why it is asserted here: the guarantee lives in the caller, and
    /// a containment check that trusts its caller is one that fails silently the day a new
    /// caller is wrong. Before this was refused, `("/etc", "passwd")` answered `etc/passwd`.
    #[test]
    fn an_absolute_cwd_is_refused_rather_than_reinterpreted() {
        assert_eq!(
            resolve_under(Some("/etc"), "passwd")
                .expect_err("an absolute cwd is not a workspace path")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn an_argument_resolves_under_the_cwd() {
        assert_eq!(
            resolve_under(Some("work/sub"), "x.txt").unwrap(),
            PathBuf::from("work/sub/x.txt")
        );
    }

    #[test]
    fn dot_dot_inside_the_tree_is_normalized() {
        assert_eq!(
            resolve_under(Some("work/sub"), "../x.txt").unwrap(),
            PathBuf::from("work/x.txt")
        );
    }

    #[test]
    fn dot_dot_past_the_root_is_refused() {
        assert!(resolve_under(Some("work"), "../../etc/passwd").is_err());
    }

    /// **The case that makes one name mean two files.**
    ///
    /// A kernel looks each component up, so a command running in the same directory cannot open
    /// any of these — `nope` is not there to descend into and come back out of. Resolving them
    /// on paper would hand the delegated executable a file its own caller could not reach.
    ///
    /// Measured against the real thing before this was refused: `cat nope/../notes.txt` in a
    /// directory holding `notes.txt` answers `No such file or directory`.
    #[test]
    fn dot_dot_through_a_name_the_argument_invented_is_refused() {
        for arg in [
            "nope/../notes.txt",
            "nope/nope/../../notes.txt",
            "sub/nope/../report.md",
            // Absolute too: the leading `/` says where to start, not that what follows exists.
            "/nope/../notes.txt",
        ] {
            assert!(
                resolve_under(Some("work/sub"), arg).is_err(),
                "{arg} resolved to something"
            );
        }
    }

    /// And the one that is not a guess still works: every component of `cwd` is a directory the
    /// command was standing in, so popping one names somewhere that was there.
    #[test]
    fn dot_dot_out_of_the_directory_it_stood_in_is_kept() {
        assert_eq!(
            resolve_under(Some("work/sub"), "../../notes.txt").unwrap(),
            PathBuf::from("notes.txt")
        );
        // Popped and then descended again — still only ever through `cwd`'s own components.
        assert_eq!(
            resolve_under(Some("work/sub"), "../other/x.txt").unwrap(),
            PathBuf::from("work/other/x.txt")
        );
    }

    /// A `cwd` is the output of `relativize` and cannot contain these. Refused rather than
    /// walked, for the reason an absolute `cwd` is: a caller who is wrong about it finds out.
    #[test]
    fn a_cwd_that_is_not_a_plain_relative_path_is_refused() {
        for cwd in ["/etc", "../up", "work/../.."] {
            assert!(
                resolve_under(Some(cwd), "x.txt").is_err(),
                "cwd {cwd} was accepted"
            );
        }
    }

    /// An absolute argument is workspace-absolute, so the cwd does not apply.
    #[test]
    fn an_absolute_argument_ignores_the_cwd() {
        assert_eq!(
            resolve_under(Some("work/sub"), "/docs/x.txt").unwrap(),
            PathBuf::from("docs/x.txt")
        );
    }

    #[test]
    fn a_relative_argument_with_no_cwd_is_refused() {
        assert!(resolve_under(None, "x.txt").is_err());
    }

    #[test]
    fn an_absolute_argument_with_no_cwd_is_fine() {
        assert_eq!(
            resolve_under(None, "/docs/x.txt").unwrap(),
            PathBuf::from("docs/x.txt")
        );
    }

    /// The workspace root is the empty path, so an argument resolved from it is just the
    /// argument — and `.` in either half does not survive.
    #[test]
    fn the_root_cwd_and_a_dot_leave_nothing_behind() {
        assert_eq!(
            resolve_under(Some(""), "x.txt").unwrap(),
            PathBuf::from("x.txt")
        );
        assert_eq!(
            resolve_under(Some("work"), "./x.txt").unwrap(),
            PathBuf::from("work/x.txt")
        );
        assert_eq!(
            resolve_under(Some("work"), ".").unwrap(),
            PathBuf::from("work")
        );
    }
}
