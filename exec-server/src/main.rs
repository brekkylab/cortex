//! `exec-server` — the host forward-server entry point.
//!
//! Serves `POST /exec` against a [`Workspace`] using the demo `Bin`, so a guest
//! `wsx` can run predefined executables on the host. Config via env:
//! `WSX_LISTEN=host:port` (default `127.0.0.1:8080`), `WSX_TOKEN=<x-cortex-token>`,
//! `WSX_ROOT=<dir>` (back the workspace with a real directory; in-memory if unset).
//!
//! Hand-rolled HTTP/1.1 over `std::net`, single-threaded — enough for a
//! coarse-grained exec channel; per-token workspace scoping and process wiring
//! are the deployment layer's job.
//!
//! TODO(security): `WSX_TOKEN` is a static pre-shared secret and an unset token
//! means auth is OFF (fail-open). A real deployment should mint a per-run token,
//! bind an ephemeral port, and treat a missing token as fail-closed; the channel
//! travels in plaintext, so it must be trusted (or add TLS).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use cortex::{Bin, ExecOutput, ExecRequest, InMemVolume, PassthroughVolume, Workspace};

fn main() -> std::io::Result<()> {
    let addr = std::env::var("WSX_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let token = std::env::var("WSX_TOKEN").ok();

    // cortex's `Mountable` data plane is async; this sync server drives each
    // exec by blocking on a runtime.
    let rt = tokio::runtime::Runtime::new()?;

    let bin = Bin::demo();
    // Root: a real directory if `WSX_ROOT` is set, else in-memory scratch.
    let mut ws = match std::env::var("WSX_ROOT") {
        Ok(root) => Workspace::new()
            .try_with_mount("", PassthroughVolume::new(root))
            .expect("mount WSX_ROOT at workspace root"),
        Err(_) => Workspace::new()
            .try_with_mount("", InMemVolume::new())
            .expect("mount in-memory root"),
    };
    // Surface the executables' docs as files under `skills/`, so a guest reads
    // them straight off the workspace.
    ws = ws
        .try_with_mount("skills", bin.as_dir())
        .expect("mount skill docs");

    let listener = TcpListener::bind(&addr)?;
    eprintln!(
        "exec-server listening on {addr} ({} executables){}",
        bin.names().count(),
        if token.is_some() {
            ", token required"
        } else {
            ""
        }
    );

    serve(&listener, token.as_deref(), |req| {
        rt.block_on(bin.invoke(&ws, &req.name, req.args))
            .map_err(|_| HandlerError::NotFound)
    })
}

/// Cap the request body so a bogus `Content-Length` can't trigger a huge alloc.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// Why the handler declined to produce output; mapped to an HTTP status.
enum HandlerError {
    /// Executable name is not registered (allowlist boundary) → 404.
    NotFound,
}

/// Accept connections forever, handling each `POST /exec` with `handler`.
fn serve(
    listener: &TcpListener,
    token: Option<&str>,
    handler: impl Fn(ExecRequest) -> Result<ExecOutput, HandlerError>,
) -> io::Result<()> {
    for stream in listener.incoming() {
        // A bad single connection must not tear down the server.
        let _ = handle_connection(stream?, token, &handler);
    }
    Ok(())
}

/// Handle one connection: parse the request, run `handler`, write the response.
fn handle_connection(
    mut stream: TcpStream,
    token: Option<&str>,
    handler: &impl Fn(ExecRequest) -> Result<ExecOutput, HandlerError>,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let mut content_length = 0usize;
    let mut got_token: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.parse().unwrap_or(0),
                "x-cortex-token" => got_token = Some(value.to_string()),
                _ => {}
            }
        }
    }

    if content_length > MAX_BODY {
        return write_response(&mut stream, 413, "Payload Too Large", b"body too large");
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;

    if !request_line.starts_with("POST /exec") {
        return write_response(&mut stream, 404, "Not Found", b"unknown route");
    }
    if let Some(expected) = token
        && got_token.as_deref() != Some(expected)
    {
        return write_response(&mut stream, 401, "Unauthorized", b"bad token");
    }
    let req = match ExecRequest::decode(&body[..]) {
        Ok(req) => req,
        Err(_) => return write_response(&mut stream, 400, "Bad Request", b"malformed request"),
    };
    match handler(req) {
        Ok(out) => write_response(&mut stream, 200, "OK", &out.encode()),
        Err(HandlerError::NotFound) => {
            write_response(&mut stream, 404, "Not Found", b"unknown executable")
        }
    }
}

fn write_response(stream: &mut TcpStream, status: u16, reason: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
