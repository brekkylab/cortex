use super::*;
use std::path::{Path, PathBuf};

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
    assert!(matches!(
        resolve_under(Some("/etc"), "passwd"),
        Err(CortexError::InvalidArgument)
    ));
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
