//! `docstore` against a directory on this host.
//!
//! The same [`DocStore`] a consumer registers, driven from a shell instead: the working
//! directory stands in for the mounted tree, so `docstore ingest notes.db .` reads what is
//! beside whatever else is in it. Everything else — the parsing, the walk, the answers — is the
//! code a consumer runs, because there is only one of it.
//!
//! What it does *not* stand in for is the mount. A consumer that mounts a tree serves a
//! `FileSystem` through FUSE, and the files are real ones on it; here the tree is already
//! real. That is the only difference, and it is deliberately the only one.

use std::{io::Write as _, process::ExitCode};

use cortex::exec::{ExecCall, Executable as _};
// The tree as a program supplies it: a directory on this host, with nothing mounted over it.
use cortex_exec_storebase::command::Here;
use cortex_exec_docstore::DocStore;

// A single-threaded runtime: the only thing awaited here is the blocking pool, which
// `spawn_blocking` has either way.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let call = ExecCall {
        name: "docstore".into(),
        // `argv[0]` is this program, and `ExecCall::args` is everything after the name — the
        // same slice an `ExecCall` carries, so the parser sees exactly what it sees there.
        args: std::env::args().skip(1).collect(),
        // The root itself: a shell's own directory is where relative names are already resolved
        // from, and `Here` below says the tree starts there.
        cwd: Some(String::new()),
        // This process's own, which is what a shell would have handed a program it ran — and
        // here that shell *is* the executor, so the two are not different machines.
        env: std::env::vars().collect(),
    };

    // Where the tree is. Nothing can be resolved without it — a store is named in the workspace
    // and opened by host path — so a directory this process cannot even name is the end of the
    // run rather than something to guess at.
    let here = match std::env::current_dir() {
        Ok(dir) => Here(dir),
        Err(e) => {
            eprintln!(
                "docstore: this process has no working directory to resolve names against: {e}"
            );
            return ExitCode::FAILURE;
        }
    };

    let result = DocStore::new().exec(&call, Some(&here)).await;

    // Written as bytes: this is program output on its way to whatever the shell pointed at, and
    // a caller piping it into something byte-oriented has to get what was produced.
    std::io::stdout().write_all(&result.stdout).ok();
    std::io::stderr().write_all(&result.stderr).ok();
    ExitCode::from(result.exit_code.clamp(0, 255) as u8)
}
