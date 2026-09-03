//! `memstore` against a directory on this host.
//!
//! The same [`MemStore`] a consumer registers, driven from a shell instead: the working
//! directory stands in for the mounted tree. Everything else — the parsing, the store, the
//! answers — is the code a consumer runs, because there is only one of it.

use std::{io::Write, process::ExitCode};

use cortex::exec::{ExecCall, Executable as _};
// The tree as a program supplies it: a directory on this host, with nothing mounted over it.
use cortex_exec_storebase::command::Here;
use cortex_exec_memstore::MemStore;

// A single-threaded runtime with no I/O driver, because this needs neither: the one thing
// awaited here is a store, and a store's blocking work happens in the calling thread. Nothing
// else in the process waits on anything.
//
// No `.env`: nothing in this crate reads a key or reaches a provider, so there is no file of
// secrets for it to want.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let call = ExecCall {
        name: "memstore".into(),
        args: std::env::args().skip(1).collect(),
        // The root of the tree, because that is where this was invoked: names resolve against
        // the directory `memstore` is standing in, which is the one `Here` is rooted at.
        cwd: Some(String::new()),
        // Run as a program rather than invoked by a consumer, so the caller's environment is this
        // process's own and there is nothing to carry across.
        env: std::env::vars().collect(),
    };

    // Where the tree is. Nothing can be resolved without it — a store is named in the workspace
    // and opened by host path — so a directory this process cannot even name is the end of the
    // run rather than something to guess at.
    let here = match std::env::current_dir() {
        Ok(dir) => Here(dir),
        Err(e) => {
            eprintln!(
                "memstore: this process has no working directory to resolve names against: {e}"
            );
            return ExitCode::FAILURE;
        }
    };

    let result = MemStore::new().exec(&call, Some(&here)).await;

    // Written as bytes: this is program output on its way to whatever the shell pointed at,
    // and a caller piping it into something byte-oriented has to get what was produced.
    std::io::stdout().write_all(&result.stdout).ok();
    std::io::stderr().write_all(&result.stderr).ok();
    ExitCode::from(result.exit_code.clamp(0, 255) as u8)
}
