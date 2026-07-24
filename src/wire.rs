//! Wire types + framing for the Executable forwarding channel: the request
//! ([`ExecRequest`]) and its process-like result ([`ExecOutput`]).
//!
//! Length-prefixed binary framing so raw `stdout`/`stderr` bytes travel verbatim
//! (no base64, unlike JSON). This is the shared contract both ends of the
//! transport agree on — the reference server/client in `cortex-rpc` encode/decode
//! these exact types — so it lives in the core, not the transport crate.

use std::io::{self, Read};

/// Reject absurd length prefixes so a bogus/hostile frame can't allocate wildly.
const MAX_FIELD: u32 = 64 * 1024 * 1024;

/// A forwarded execution request: which executable, with which args.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecRequest {
    pub name: String,
    pub args: Vec<String>,
}

/// The result of running an executable, carried back over the wire.
///
/// Process-like (`code`/`stdout`/`stderr`) so the guest `wsx` can replay the
/// streams and exit with `code`, making a forwarded run feel like a local one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ExecOutput {
    /// A success (`code == 0`) carrying `stdout` and empty `stderr`.
    pub fn ok(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            code: 0,
            stdout: stdout.into(),
            stderr: Vec::new(),
        }
    }
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn get_bytes(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_FIELD {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "field too large"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn get_str(r: &mut impl Read) -> io::Result<String> {
    let b = get_bytes(r)?;
    String::from_utf8(b).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf-8"))
}

fn get_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut v = [0u8; 4];
    r.read_exact(&mut v)?;
    Ok(u32::from_le_bytes(v))
}

impl ExecRequest {
    /// Encode as `name` then a `u32` arg count then each arg (all length-prefixed).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_bytes(&mut out, self.name.as_bytes());
        out.extend_from_slice(&(self.args.len() as u32).to_le_bytes());
        for a in &self.args {
            put_bytes(&mut out, a.as_bytes());
        }
        out
    }

    pub fn decode(mut r: impl Read) -> io::Result<Self> {
        let name = get_str(&mut r)?;
        let count = get_u32(&mut r)?;
        if count > MAX_FIELD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "arg count too large"));
        }
        let mut args = Vec::with_capacity(count as usize);
        for _ in 0..count {
            args.push(get_str(&mut r)?);
        }
        Ok(Self { name, args })
    }
}

impl ExecOutput {
    /// Encode as `i32` code then length-prefixed `stdout` then `stderr`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.code.to_le_bytes());
        put_bytes(&mut out, &self.stdout);
        put_bytes(&mut out, &self.stderr);
        out
    }

    pub fn decode(mut r: impl Read) -> io::Result<Self> {
        let code = i32::from_le_bytes({
            let mut v = [0u8; 4];
            r.read_exact(&mut v)?;
            v
        });
        let stdout = get_bytes(&mut r)?;
        let stderr = get_bytes(&mut r)?;
        Ok(Self {
            code,
            stdout,
            stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let req = ExecRequest {
            name: "cat".into(),
            args: vec!["a.txt".into(), "".into(), "유니코드".into()],
        };
        let decoded = ExecRequest::decode(&req.encode()[..]).unwrap();
        assert_eq!(decoded, req);
    }

    #[test]
    fn request_no_args() {
        let req = ExecRequest {
            name: "ls".into(),
            args: vec![],
        };
        assert_eq!(ExecRequest::decode(&req.encode()[..]).unwrap(), req);
    }

    #[test]
    fn output_round_trip_with_raw_bytes() {
        // Non-UTF-8 bytes must survive verbatim (the reason we avoid JSON).
        let out = ExecOutput {
            code: 3,
            stdout: vec![0, 159, 146, 150, 255],
            stderr: b"boom".to_vec(),
        };
        assert_eq!(ExecOutput::decode(&out.encode()[..]).unwrap(), out);
    }

    #[test]
    fn oversized_field_rejected() {
        // len prefix claims 4 GiB but no body follows → InvalidData, not OOM.
        let mut frame = u32::MAX.to_le_bytes().to_vec();
        frame.extend_from_slice(b"short");
        let err = ExecRequest::decode(&frame[..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
