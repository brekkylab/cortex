use std::{
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

use cortex::{
    exec::{ExecCall, Executable as _},
    fs::Mount,
};
use cortex_exec_mem::Mem;

/// The tree, as the program supplies it: the directory `mem` was run in.
///
/// A [`Mount`] with nothing mounted. What the trait offers a consumer is one fact — where the
/// tree is on this host — and when the tree already *is* a directory on this host, that fact is
/// known before anything is asked: there is no binding to make, and so nothing for a drop to
/// take down. The RAII rule holds trivially rather than being broken.
///
/// This is the whole difference between `mem` here and `mem` under a console. There it is
/// handed a mount over a tree it could not otherwise reach; here it is handed this. The
/// `Executable` is the same one and cannot tell which it got, which is what makes running it as
/// a program a real test of what the console does.
struct Here(PathBuf);

impl Mount for Here {
    fn mountpoint(&self) -> &Path {
        &self.0
    }
}

// A single-threaded runtime with no I/O driver, because this needs neither: the one thing
// awaited here is a store, and a store's blocking work goes to the blocking pool, which is not
// the runtime's threads. Nothing else in the process waits on anything.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // `.env`, from here upward, before anything reads the environment.
    //
    // What `mem` needs from the environment is a provider and the key that pays for it, and
    // those are the two things nobody wants in shell history or in a session's exported set.
    // A file beside the tree is where they already live for everything else that talks to a
    // model, so this reads the same one rather than inventing a place of its own.
    //
    // It does not override what is already exported: a variable set on the command line is a
    // deliberate act for one run, and a file on disk should not quietly win over it.
    //
    // This is the program's doing and not `Mem`'s. Delegated in a console, `mem` is handed an
    // environment by whoever called it — reading a file off the host's disk and pushing it
    // into the process would be a library changing the world behind its caller's back.
    match dotenvy::dotenv() {
        Ok(_) => {}
        // No `.env` is the ordinary case, not a complaint.
        Err(e) if e.not_found() => {}
        // Said, and then carried on: the environment is the input, and a file is one source of
        // it among several. A caller who exported the variables directly is not stopped by a
        // file they may not even know is there — but a caller who wrote it and mistyped a line
        // would otherwise learn about it as an unrelated "not set" much later.
        Err(e) => eprintln!("mem: .env: {e}"),
    }

    let call = ExecCall {
        name: "mem".into(),
        args: std::env::args().skip(1).collect(),
        // The root of the tree, because that is where this was invoked: names resolve against
        // the directory `mem` is standing in, which is the one `Cwd` is rooted at.
        cwd: Some(String::new()),
        // Run as a program rather than called in-process, so the caller's environment is this
        // process's own and there is nothing to carry across.
        env: std::env::vars().collect(),
    };

    // Where the tree is. Nothing can be resolved without it — a store is named in the workspace
    // and opened by host path — so a directory this process cannot even name is the end of the
    // run rather than something to guess at.
    let here = match std::env::current_dir() {
        Ok(dir) => Here(dir),
        Err(e) => {
            eprintln!("mem: this process has no working directory to resolve names against: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mem = Mem::new();
    let result = mem.exec(&call, Some(&here)).await;

    // Written as bytes: this is program output on its way to whatever the shell pointed at,
    // and a caller piping it into something byte-oriented has to get what was produced.
    std::io::stdout().write_all(&result.stdout).ok();
    std::io::stderr().write_all(&result.stderr).ok();
    ExitCode::from(result.exit_code.clamp(0, 255) as u8)
}
