//! Loopback integration test for the forwarding channel: spawn the real
//! `exec-server` binary and drive it with the guest `wsx` client over TCP.
//! Exercises the transport end to end — 200 / 404 / 401 and program failure as
//! a non-zero code — against the demo `Bin` backed by a temp-dir workspace.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::Duration;

use cortex::ExecRequest;
use wsx::post;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn scratch() -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("cortex-exec-server-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A spawned `exec-server`, killed on drop.
struct Server {
    child: Child,
    addr: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(token: &str, root: &PathBuf) -> Server {
    let addr = format!("127.0.0.1:{}", free_port());
    let child = Command::new(env!("CARGO_BIN_EXE_exec-server"))
        .env("WSX_LISTEN", &addr)
        .env("WSX_TOKEN", token)
        .env("WSX_ROOT", root)
        .spawn()
        .expect("spawn exec-server");
    // Wait until the listener accepts.
    for _ in 0..100 {
        if TcpStream::connect(&addr).is_ok() {
            break;
        }
        sleep(Duration::from_millis(20));
    }
    Server { child, addr }
}

fn req(name: &str, args: &[&str]) -> ExecRequest {
    ExecRequest {
        name: name.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn forwarding_round_trip() {
    let root = scratch();
    let server = start("secret", &root);
    let addr = server.addr.as_str();
    let tok = Some("secret");

    // write then read back: 200, code 0.
    let out = post(addr, tok, &req("write", &["f", "hi"])).unwrap();
    assert_eq!(out.code, 0);
    let out = post(addr, tok, &req("cat", &["f"])).unwrap();
    assert_eq!(out.stdout, b"hi");

    // program failure (missing file): 200 with a non-zero code, not a transport error.
    let out = post(addr, tok, &req("cat", &["missing"])).unwrap();
    assert_eq!(out.code, 1);

    // unknown name: allowlist boundary -> 404.
    let err = post(addr, tok, &req("nope", &[])).unwrap_err();
    assert!(err.to_string().contains("404"));

    // bad token -> 401.
    let err = post(addr, Some("wrong"), &req("cat", &["f"])).unwrap_err();
    assert!(err.to_string().contains("401"));
}
