//! `exec-server` — the host forward-server entry point.
//!
//! Serves `POST /exec` against a [`Workspace`] using the demo registry, so a
//! guest `wsx` can run predefined executables on the host. Config via env:
//! `WSX_LISTEN=host:port` (default `127.0.0.1:8080`), `WSX_TOKEN=<x-cortex-token>`,
//! `WSX_ROOT=<dir>` (back the workspace with a real directory; in-memory if unset).
//!
//! NOTE: single workspace / single-threaded — enough to run the channel on the
//! host. Per-token workspace scoping and process launch/wiring belong to the
//! deployment layer, out of scope here.
//!
//! TODO(security): token management is the deployment layer's job too. `WSX_TOKEN`
//! here is a *static* pre-shared secret, and an unset token means auth is OFF
//! (fail-open). A real deployment should mint a fresh *per-run* token, bind an
//! ephemeral port, and inject both into the guest (e.g. a per-run UUID),
//! and treat a missing token as fail-closed. It also travels in plaintext, so the
//! guest↔host channel must be trusted (or add TLS).

use std::net::TcpListener;

use cortex::{ExecutableRegistry, PassthroughVolume, Workspace};
use cortex_rpc::server::{HandlerError, serve};

fn main() -> std::io::Result<()> {
    let addr = std::env::var("WSX_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let token = std::env::var("WSX_TOKEN").ok();

    let mut ws = Workspace::new();
    if let Ok(root) = std::env::var("WSX_ROOT") {
        ws = ws
            .try_with_mountable("", Box::new(PassthroughVolume::new(root)))
            .expect("mount WSX_ROOT at workspace root");
    }
    let reg = ExecutableRegistry::demo();

    let listener = TcpListener::bind(&addr)?;
    eprintln!(
        "exec-server listening on {addr} ({} executables){}",
        reg.names().count(),
        if token.is_some() {
            ", token required"
        } else {
            ""
        }
    );

    serve(&listener, token.as_deref(), |req| {
        reg.invoke(&ws, &req.name, req.args)
            .map_err(|_| HandlerError::NotFound)
    })
}
