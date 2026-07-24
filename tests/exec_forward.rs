//! Loopback integration test for the VM→host Executable forwarding channel:
//! a real `POST /exec` over TCP, dispatched through an [`ExecutableRegistry`]
//! against a [`Workspace`].
//!
//! `Workspace` is single-threaded (`!Send`), so the server side (which touches
//! it) stays on the test thread while the client runs on a spawned thread.

use std::io;
use std::net::TcpListener;
use std::path::Path;
use std::thread;

use cortex::{ExecOutput, Executable, ExecutableRegistry, Mountable, Workspace};
use cortex_rpc::ExecRequest;
use cortex_rpc::client;
use cortex_rpc::server::{HandlerError, handle_connection};

/// Prints `args[0]` from the workspace; errors (e.g. missing file) surface as
/// the executable's own failure.
struct Cat;
impl Executable for Cat {
    fn name(&self) -> &str {
        "cat"
    }
    fn summary(&self) -> &str {
        "print a file"
    }
    fn usage(&self) -> &str {
        "<path>"
    }
    fn skill(&self) -> String {
        "# cat\n`wsx cat <path>` — print a file".into()
    }
    fn exec(&self, ws: &Workspace, args: Vec<String>) -> cortex::Result<ExecOutput> {
        Ok(ExecOutput::ok(ws.read(Path::new(&args[0]))?))
    }
}

/// One real request/response over loopback TCP. Server handler bridges the
/// registry (unknown name → 404); client runs on a thread.
fn round_trip(
    reg: &ExecutableRegistry,
    ws: &Workspace,
    server_token: Option<&str>,
    client_token: Option<&str>,
    req: ExecRequest,
) -> io::Result<ExecOutput> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let ct = client_token.map(String::from);
    let client = thread::spawn(move || client::post(addr, ct.as_deref(), &req));

    let (stream, _) = listener.accept().unwrap();
    handle_connection(stream, server_token, &|r: ExecRequest| {
        reg.invoke(ws, &r.name, r.args)
            .map_err(|_| HandlerError::NotFound)
    })
    .unwrap();

    client.join().unwrap()
}

fn cat_registry() -> ExecutableRegistry {
    ExecutableRegistry::new().register(Cat)
}

fn ws_with_file() -> Workspace {
    let ws = Workspace::new();
    ws.write(Path::new("f"), b"hi").unwrap();
    ws
}

#[test]
fn forwarded_run_returns_output() {
    let out = round_trip(
        &cat_registry(),
        &ws_with_file(),
        None,
        None,
        ExecRequest {
            name: "cat".into(),
            args: vec!["f".into()],
        },
    )
    .unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.stdout, b"hi");
}

#[test]
fn executable_failure_comes_back_as_nonzero_code() {
    // Missing file: the run fails, but that is program failure — a 200 response
    // with code != 0, not a transport error.
    let out = round_trip(
        &cat_registry(),
        &Workspace::new(),
        None,
        None,
        ExecRequest {
            name: "cat".into(),
            args: vec!["missing".into()],
        },
    )
    .unwrap();
    assert_eq!(out.code, 1);
    assert!(!out.stderr.is_empty());
}

#[test]
fn unknown_executable_is_rejected() {
    let err = round_trip(
        &cat_registry(),
        &Workspace::new(),
        None,
        None,
        ExecRequest {
            name: "nope".into(),
            args: vec![],
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("404"));
}

#[test]
fn bad_token_is_rejected() {
    let err = round_trip(
        &cat_registry(),
        &ws_with_file(),
        Some("secret"),
        Some("wrong"),
        ExecRequest {
            name: "cat".into(),
            args: vec!["f".into()],
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("401"));
}

#[test]
fn registry_emits_agent_md_and_skills() {
    let reg = cat_registry();
    let md = reg.agent_md("/root/skills");
    assert!(md.contains("wsx <name> [args...]"));
    assert!(md.contains("`cat`"));
    assert!(md.contains("/root/skills/cat/SKILL.md"));

    let skills = reg.skills("/root/skills");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].0, "/root/skills/cat/SKILL.md");
    assert!(skills[0].1.contains("wsx cat"));
}
