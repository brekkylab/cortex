use super::*;
use std::path::PathBuf;

fn call(cwd: Option<&str>, args: &[&str]) -> ExecCall {
    ExecCall {
        name: "summarize".into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        cwd: cwd.map(str::to_owned),
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
