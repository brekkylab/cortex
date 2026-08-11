//! `wsx` guest client: `post` an [`ExecRequest`] to the host exec-server and
//! return its [`ExecOutput`].
//!
//! A minimal, dependency-free HTTP/1.1 `POST /exec` over `std::net`. Relies on
//! `Connection: close`: the server closes after replying, so the body is simply
//! everything after the header terminator. `post` is the only public item.

use std::{
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
};

use cortex::{ExecOutput, ExecRequest};

/// POST `req` to the forward-server at `addr`, returning its [`ExecOutput`].
/// A non-200 status becomes an `Err` carrying the status and message body.
pub fn post(
    addr: impl ToSocketAddrs,
    token: Option<&str>,
    req: &ExecRequest,
) -> io::Result<ExecOutput> {
    let body = req.encode();

    let mut head = format!(
        "POST /exec HTTP/1.1\r\nHost: cortex\r\nContent-Type: application/octet-stream\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(t) = token {
        head.push_str(&format!("x-cortex-token: {t}\r\n"));
    }
    head.push_str("\r\n");

    // Send head+body in one write so the msb host-forward proxy sees one stream.
    let mut msg = head.into_bytes();
    msg.extend_from_slice(&body);

    let mut stream = TcpStream::connect(addr)?;
    stream.write_all(&msg)?;
    stream.flush()?;

    // Read by Content-Length rather than a clean close: the msb proxy can RST
    // right after the server's `Connection: close`, so stop once the body is
    // complete and tolerate a reset arriving after we have the whole message.
    let mut resp = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                resp.extend_from_slice(&tmp[..n]);
                if let Some(end) = find(&resp, b"\r\n\r\n")
                    && let Some(len) = content_length(&resp[..end])
                    && resp.len() >= end + 4 + len
                {
                    break;
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(ref e) if e.kind() == io::ErrorKind::ConnectionReset => break,
            Err(e) => return Err(e),
        }
    }

    let sep = find(&resp, b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header terminator"))?;
    let status = parse_status(&resp[..sep])?;
    let body = &resp[sep + 4..];
    if status != 200 {
        return Err(io::Error::other(format!(
            "server status {status}: {}",
            String::from_utf8_lossy(body)
        )));
    }
    ExecOutput::decode(body)
}

/// Parse the status code out of `HTTP/1.1 <code> <reason>`.
fn parse_status(head: &[u8]) -> io::Result<u16> {
    let line = head
        .split(|&b| b == b'\r' || b == b'\n')
        .next()
        .unwrap_or(&[]);
    let line = std::str::from_utf8(line).unwrap_or_default();
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no status code"))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Parse a `Content-Length` header value (case-insensitive).
fn content_length(head: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(head).ok()?;
    text.split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}
