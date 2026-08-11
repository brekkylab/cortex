//! `wsx` — the guest-side CLI.
//!
//! A VM agent's bash tool runs `wsx <name> [args...]`; this forwards the request
//! to the host forward-server, which runs the named executable against the
//! shared workspace and returns its output. `wsx` replays that output and exits
//! with the executable's code, so a forwarded run feels like a local program.
//!
//! Config via env: `WSX_HOST=host:port`, `WSX_TOKEN=<x-cortex-token>`.

use std::{io::Write, process::ExitCode};

use cortex::ExecRequest;
use wsx::post;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(name) = args.next() else {
        eprintln!("usage: wsx <name> [args...]   (env: WSX_HOST=host:port, WSX_TOKEN=…)");
        return ExitCode::from(2);
    };
    let req = ExecRequest {
        name,
        args: args.collect(),
    };

    // Accept a bare host:port or an `http://…/` form.
    let host =
        std::env::var("WSX_HOST").unwrap_or_else(|_| "host.microsandbox.internal:8080".into());
    let host = host
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let token = std::env::var("WSX_TOKEN").ok();

    match post(host.as_str(), token.as_deref(), &req) {
        Ok(out) => {
            let _ = std::io::stdout().write_all(&out.stdout);
            let _ = std::io::stderr().write_all(&out.stderr);
            ExitCode::from(out.code.clamp(0, 255) as u8)
        }
        Err(e) => {
            eprintln!("wsx: {e}");
            ExitCode::from(1)
        }
    }
}
