//! `mem` against a directory on this host.
//!
//! The same [`Mem`] a console delegates to, driven from a shell instead: the working directory
//! stands in for the mounted tree, so `mem insert notes.sqlite "..."` writes the store beside
//! whatever else is in it. Everything else — the parsing, the pipeline, the answers — is the
//! code that runs under a console, because there is only one of it.
//!
//! What it does *not* stand in for is the mount. A console's tree is a `Mountable` served
//! through FUSE, and the store is a real file on it; here the tree is already real. That is the
//! only difference, and it is deliberately the only one.
//!
//! The providers are the offline pair, [`HashEmbedder`] and [`Verbatim`]: this binary exists to
//! exercise the store, and reaching a service would make it exercise a network instead.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use cortex::{
    executable::{ExecCall, Executable},
    fs::Mount,
};
use cortex_exec_mem::{HashEmbedder, Mem, Verbatim};

/// The directory this was run in, as the tree `mem` resolves names against.
struct Cwd(PathBuf);

impl Mount for Cwd {
    fn mountpoint(&self) -> &Path {
        &self.0
    }
}

fn main() -> ExitCode {
    // A single-threaded runtime with no I/O driver, because this needs neither: the one thing
    // awaited here is a store, and a store's blocking work goes to the blocking pool, which is
    // not the runtime's threads. Nothing else in the process waits on anything.
    let runtime = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("mem: no runtime to run on: {e}");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(run())
}

async fn run() -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => Cwd(cwd),
        Err(e) => {
            eprintln!("mem: this process has no working directory to use as a tree: {e}");
            return ExitCode::from(1);
        }
    };

    let call = ExecCall {
        name: "mem".into(),
        args: std::env::args().skip(1).collect(),
        // The root of the tree, because that is where this was invoked: names resolve against
        // the directory `mem` is standing in, which is the one `Cwd` is rooted at.
        cwd: Some(String::new()),
        // Run as a program rather than delegated to, so the caller's environment is this
        // process's own and there is nothing to carry across.
        env: std::env::vars().collect(),
    };

    let mem = Mem::new(Arc::new(HashEmbedder::default()), Arc::new(Verbatim));
    let result = mem.exec(&call, Some(&cwd)).await;

    // Written as bytes: this is program output on its way to whatever the shell pointed at,
    // and a caller piping it into something byte-oriented has to get what was produced.
    std::io::stdout().write_all(&result.stdout).ok();
    std::io::stderr().write_all(&result.stderr).ok();
    ExitCode::from(result.exit_code.clamp(0, 255) as u8)
}
