//! The server side of the shim wire.
//!
//! Runs on its own thread for the whole life of the server, because the main
//! thread spends that life blocked: first on stdin waiting for a request, then
//! on the child it spawned for one. A shim is a descendant of that child, so
//! the reply it is waiting for can only come from somewhere else.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::thread;

use crate::ipc::{Call, CallReply};
use crate::registry::Registry;

/// A name that reached us without being registered. Not reachable by an honest
/// caller — the only names on `PATH` are the ones we linked — so this answers
/// something that spoke to the socket directly.
const NOT_FOUND: i32 = 127;

/// Accept shim calls until the listener goes away.
pub fn serve(listener: UnixListener, registry: Arc<Registry>) {
    for stream in listener.incoming() {
        // One bad connection says nothing about the next.
        let Ok(stream) = stream else { continue };
        let registry = Arc::clone(&registry);
        // A thread per call, because two shims can be in flight at once:
        // `foo | bar` starts both. Served serially, the first could block
        // writing into a pipe nobody is draining yet while the second waits in
        // the accept backlog to become the drainer — a deadlock with no timeout
        // to break it.
        thread::spawn(move || {
            let _ = handle(stream, &registry);
        });
    }
}

/// Read one [`Call`], run it, write one [`CallReply`].
fn handle(stream: UnixStream, registry: &Registry) -> std::io::Result<()> {
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;

    let call: Call = match serde_json::from_str(&line) {
        Ok(call) => call,
        // Nothing to answer to — we do not know who was asking or for what.
        Err(_) => return Ok(()),
    };

    let reply = match registry.invoke(&call.name, call.args) {
        Some(out) => CallReply {
            stdout: out.stdout,
            stderr: out.stderr,
            code: out.exit_code,
        },
        None => CallReply {
            stdout: String::new(),
            stderr: format!("{}: not a registered executable\n", call.name),
            code: NOT_FOUND,
        },
    };

    let mut writer = &stream;
    serde_json::to_writer(&mut writer, &reply)?;
    writer.write_all(b"\n")?;
    writer.flush()
}
