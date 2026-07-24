//! Minimal, dependency-free HTTP/1.1 server for the `POST /exec` channel.
//!
//! Hand-rolled over `std::net` (no framework) to keep the crate dep-free.
//! Single-threaded: [`serve`] handles one
//! connection at a time — enough for a coarse-grained exec channel — which also
//! sidesteps needing the handler (and any `Workspace` it captures) to be `Send`.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use crate::{ExecOutput, ExecRequest};

/// Cap the request body so a bogus `Content-Length` can't trigger a huge alloc.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// Why the handler declined to produce output; mapped to an HTTP status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandlerError {
    /// Executable name is not registered (allowlist boundary) → 404.
    NotFound,
    /// Backend/infra failure → 500.
    Internal,
}

/// Accept connections forever, handling each `POST /exec` with `handler`.
///
/// If `token` is `Some`, requests must carry a matching `x-cortex-token`; `None`
/// disables auth. The match is a plain equality check of a shared secret — this
/// reference server only gates on a token it is *given*. Minting/rotating a
/// per-run token and delivering it to the guest (and TLS if the channel isn't
/// trusted) are the deployment layer's responsibility.
pub fn serve(
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

/// Handle exactly one connection: parse the request, run `handler`, write the
/// response. Exposed so callers (and tests) can drive one request at a time.
pub fn handle_connection(
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
        Err(HandlerError::Internal) => {
            write_response(&mut stream, 500, "Internal Server Error", b"internal error")
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
