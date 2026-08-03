//! What every Cortex console server is built on — behind the `console_server`
//! feature.
//!
//! A *console server* is a process that accepts commands over stdio and runs
//! them somewhere — on this host, inside a micro-VM, wherever. What differs
//! between them is only that "somewhere"; the wire they speak and the loop they
//! serve it with are the same, and both live here. An implementation turns the
//! feature on, supplies a `handle` function, and is done.
//!
//! Keeping it in one place is the point: the parts a caller depends on — the
//! shape of a request, the shape of a reply, the one-in-one-out ordering — are
//! decided once, so two servers cannot drift apart on them. See [`Request`] and
//! [`Response`] for the wire, [`enter_loop`] for the loop.
//!
//! It is feature-gated because most of `cortex` is about what a backend *does*,
//! not about being spoken to over a pipe: a consumer that only wants
//! [`Executable`](crate::Executable) or [`volume`](crate::volume) has no use for
//! this module, and should not pay `serde_json` for it.

use std::io::{self, BufRead, Write};

use crate::console::{Request, Response};

/// Serve requests off stdin until the caller says to stop.
///
/// One line in, one line out, in lockstep — `handle` is called for a request
/// only after the previous response has been flushed, so a caller that writes
/// then reads never deadlocks and never has to match replies to requests.
///
/// Two things end the loop, both of them normal: an explicit
/// [`Request::Quit`], and stdin reaching EOF because the caller closed the pipe
/// or went away. `handle` sees neither — `Quit` is the protocol's business, not
/// the backend's, so it is answered here by returning.
///
/// A request that will not parse is answered with [`Response::Error`] and the
/// loop continues: one bad line says nothing about the next one, and the caller
/// is still waiting for a reply to it. Only a broken stream — unreadable stdin,
/// unwritable stdout — returns `Err`, because at that point no reply can reach
/// anyone.
///
/// `teardown` runs once as the loop ends, whichever way it ends — quit, EOF, a
/// broken stream, or a panic out of `handle`. Whatever the backend acquired
/// before serving is released there, so the release does not have to be
/// repeated at each of the exits.
pub fn enter_loop(
    handle: impl Fn(Request) -> Response,
    teardown: impl Fn() -> (),
) -> anyhow::Result<()> {
    // A guard rather than a call at each `return`: it also covers an unwinding
    // panic, which matters when what needs releasing is a VM or a mount rather
    // than memory the OS would reclaim for us anyway.
    let _teardown = Teardown(teardown);

    // Both held for the whole loop rather than re-taken per line: nothing else
    // in this process touches these streams, and stdin's buffer has to persist
    // across reads or a line could be split between them.
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut line = String::new();

    loop {
        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            return Ok(());
        }
        // A blank line is spacing, not a request; nothing to answer.
        if line.trim().is_empty() {
            continue;
        }

        let res = match serde_json::from_str(&line) {
            Ok(Request::Quit) => return Ok(()),
            Ok(req) => handle(req),
            Err(e) => Response::Error {
                message: format!("malformed request: {e}"),
            },
        };

        // Newline-terminated so the caller can read a reply as a line without
        // knowing its length, and flushed because the caller is blocked on that
        // line — on a pipe nothing reaches it until the flush.
        serde_json::to_writer(&mut stdout, &res)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
}

/// Runs its function when dropped, so [`enter_loop`] has one exit path for
/// cleanup instead of one per `return`.
struct Teardown<F: Fn()>(F);

impl<F: Fn()> Drop for Teardown<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
