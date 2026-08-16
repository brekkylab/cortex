//! `index` against a directory on this host.
//!
//! The same [`Index`] a console delegates to, driven from a shell instead: the working
//! directory stands in for the mounted tree, so `index ingest notes .` reads what is beside
//! whatever else is in it. Everything else — the parsing, the walk, the answers — is the code
//! that runs under a console, because there is only one of it.
//!
//! What it does *not* stand in for is the mount. A console's tree is a `FileSystem` served
//! through FUSE, and the files are real ones on it; here the tree is already real. That is the
//! only difference, and it is deliberately the only one.
//!
//! The stores go under `$CORTEX_INDEX_ROOT`, or `.cortex-index` beside the tree. Beside and
//! not in a temporary directory, because an index a second run cannot find is one this binary
//! could not be used to try anything with — and the leading dot is what keeps the walk from
//! reading its own indexes back in.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cortex::exec::{ExecCall, Executable};
use cortex::fs::Mount;
use cortex_exec_index::Index;

/// The directory this was run in, as the tree `index` resolves names against.
struct Cwd(PathBuf);

impl Mount for Cwd {
    fn mountpoint(&self) -> &Path {
        &self.0
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("index: {e}");
            ExitCode::from(1)
        }
    }
}

fn run() -> std::io::Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let root = match std::env::var_os("CORTEX_INDEX_ROOT") {
        Some(dir) => PathBuf::from(dir),
        None => cwd.join(".cortex-index"),
    };
    let index = Index::new(root)?;

    // `argv[0]` is this program, and `ExecCall::args` is everything after the name — the same
    // slice a console hands over, so the parser sees exactly what it sees there.
    let call = ExecCall {
        name: "index".into(),
        args: std::env::args().skip(1).collect(),
        // The root itself: a shell's own directory is where relative names are already
        // resolved from, and `Cwd` above says the tree starts here.
        cwd: Some(String::new()),
        // This process's own, which is what a shell would have handed a program it ran —
        // and here that shell *is* the executor, so the two are not different machines.
        env: std::env::vars().collect(),
    };

    let mount = Cwd(cwd);
    // A current-thread runtime is enough: the only thing awaited here is the blocking pool,
    // which `spawn_blocking` has either way.
    let result = tokio::runtime::Builder::new_current_thread()
        .build()?
        .block_on(async { index.exec(&call, Some(&mount as &dyn Mount)).await });

    std::io::stdout().write_all(&result.stdout)?;
    std::io::stderr().write_all(&result.stderr)?;
    Ok(ExitCode::from(result.exit_code.clamp(0, 255) as u8))
}
