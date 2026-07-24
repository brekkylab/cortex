//! Smoke test: run the compiled `wsx` binary against a loopback forward-server
//! and confirm it replays stdout/stderr and the exit code.

use std::net::TcpListener;
use std::process::Command;
use std::thread;

use cortex_rpc::server::handle_connection;
use cortex_rpc::{ExecOutput, ExecRequest};

#[test]
fn wsx_forwards_and_replays_output() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let _ = handle_connection(stream, Some("tok"), &|r: ExecRequest| {
            // Echo the first arg to stdout, something to stderr, exit code 7.
            Ok(ExecOutput {
                code: 7,
                stdout: r.args.first().cloned().unwrap_or_default().into_bytes(),
                stderr: b"warn".to_vec(),
            })
        });
    });

    let out = Command::new(env!("CARGO_BIN_EXE_wsx"))
        .args(["greet", "hello"])
        .env("WSX_HOST", format!("{addr}"))
        .env("WSX_TOKEN", "tok")
        .output()
        .unwrap();

    server.join().unwrap();
    assert_eq!(out.stdout, b"hello");
    assert_eq!(out.stderr, b"warn");
    assert_eq!(out.status.code(), Some(7));
}
