//! Smoke test for the compiled `wsx` binary: run it against a canned loopback
//! responder (the test owns the listener) and confirm it replays the returned
//! stdout/stderr and exit code. No real server needed.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;

use cortex::ExecOutput;

#[test]
fn wsx_forwards_and_replays_output() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // Drain the request, then reply with a canned ExecOutput.
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let body = ExecOutput {
            code: 7,
            stdout: b"hello".to_vec(),
            stderr: b"warn".to_vec(),
        }
        .encode();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();
        stream.flush().unwrap();
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
