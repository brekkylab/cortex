//! Answering a console session: the wire, and what is run at the far end of it.

mod uvm;

use std::{io, path::PathBuf};

use cortex::console::stdio::StdioServer;

pub use uvm::Uvm;

/// Marks a path on the host as this server's, and is where the owning pid starts.
///
/// The shape the sweep below reads back is `{PREFIX}{pid}-{seq}-{what}`: the pid says who owns
/// the path, and the counter keeps two boots of one server apart — a `stop` and the next
/// `start` are two images, and the first is still being deleted while the second is being
/// formatted. Whatever ends up creating these paths has to spell them that way, or the sweep
/// leaks exactly the files it exists to reclaim.
///
/// Deliberately not `cortex-uvm-`, which is the first micro-VM console server's: the two can
/// be running at once, and each should only ever reclaim its own. The prefixes do not collide
/// in either direction — that one's sweep strips `cortex-uvm-` from these names and finds
/// `v2` where it wants a pid, so it skips them, and these names are the only ones this prefix
/// matches.
const PREFIX: &str = "cortex-uvm-v2-";

/// Answer a console session on stdin and stdout, until the client ends it.
pub async fn run() -> anyhow::Result<()> {
    // Taken for the life of the process: from here on stdout carries frames and nothing
    // else, which is why every diagnostic goes to stderr. Nothing enforces that — it is a
    // rule, and this is where it starts applying.
    let _server = StdioServer::stdio()?;

    // Then delete what a server that was killed outright left behind.
    //
    // A session's own values remove their files when they drop, which covers a session
    // ending any way that runs a destructor — a `stop`, a `quit`, a process exiting on
    // its own. What no destructor covers is `SIGKILL`, and what is lost to it is a sparse
    // image that may hold everything a session wrote.
    //
    // So this sweep is the only thing that ever reclaims those, and it runs at the start
    // of every server rather than at the end of any: a process that was killed is not one
    // that gets to clean up, and the next one is the first thing that can.
    //
    // Best-effort throughout, because none of it is this session's to succeed at: a path
    // that cannot be removed is left for the next run, which is where it already was.
    //
    // Both directories, because the two differ on macOS: a socket has to live under
    // `/tmp` for its path to fit in `sockaddr_un`, where everything else uses the
    // per-user temp directory.
    let mut dirs = vec![std::env::temp_dir()];
    if !dirs.contains(&PathBuf::from("/tmp")) {
        dirs.push(PathBuf::from("/tmp"));
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|n| n.strip_prefix(PREFIX))
                .and_then(|rest| rest.split('-').next())
                .and_then(|pid| pid.parse::<i32>().ok())
            else {
                continue;
            };
            // `kill(pid, 0)` sends no signal and only reports whether the pid could be
            // signalled: `ESRCH` — no such process — is the one answer that means the path
            // is abandoned, where `EPERM` says the pid is alive under another user and its
            // files are not ours to remove.
            //
            // SAFETY: signal 0 performs only that permission and existence check; it
            // cannot affect this or any other process.
            let gone = unsafe { libc::kill(pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if !gone {
                continue;
            }
            let path = entry.path();
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }

    todo!()
}
