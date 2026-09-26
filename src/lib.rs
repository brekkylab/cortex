//! # Cortex
//!
//! The environment an agent works in: what it can see, and what it can do.
//!
//! Both are ordinary to whatever runs inside them, and that is the point. An agent handed a
//! filesystem and a shell has two interfaces it already knows, and every capability added to
//! them arrives as a file to read or a command to run rather than as something else to learn.
//!
//! * **What it sees is a filesystem.** Whatever it should know about — a project directory,
//!   an object store, a Notion workspace, a tree assembled in memory — implements
//!   [`FileSystem`](fs::FileSystem). A [`Directory`](fs::Directory) is the one a caller
//!   assembles: files handed to it in memory, host directories grafted in at paths the caller
//!   chooses. A binding mounts that tree on the host, so what reads it is `cat`,
//!   `grep`, and whatever else the agent thought to run.
//! * **What it does is run commands.** A [`ConsoleClient`](console::ConsoleClient) runs them somewhere —
//!   this host, a micro-VM — over one channel: an `exec` carrying an argv, and everything the
//!   command wrote coming back.
//!
//! ## Quickstart
//!
//! Needs the `mount` feature. This is Linux, where the binding is `FuseMount`; on macOS the
//! same two lines say `FuseTMount` and on Windows `DokanMount`, and nothing else about this
//! changes.
//!
//! ```ignore
//! use std::path::Path;
//!
//! use cortex::console::ConsoleClient;
//! use cortex::fs::{Directory, FuseMount};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // What the agent can see: a file held in memory beside a host directory. A
//!     // `Directory` is itself a `FileSystem`, so a binding drives it like any single store.
//!     let context = Directory::new()
//!         .with_file("notes/today.md", "ship the release".as_bytes())?
//!         .with_mount("project", "/home/me/project")?;
//!
//!     // Where the host can see it. Constructing the guard mounts, dropping it unmounts,
//!     // and the console below holds it for the length of the session.
//!     let mount = FuseMount::try_new(context, Path::new("/tmp/session"))?;
//!
//!     // What the agent can do: a server that runs its commands, against that tree. A
//!     // session's shape is said once, when the console is built — including where each
//!     // tree appears to the commands, which is what their paths are spelled under.
//!     let mut console = ConsoleClient::builder()
//!         .mount(mount, "/work")
//!         .build()
//!         .await?;
//!
//!     // A shell the agent wrote, run wherever that server runs things.
//!     let result = console.exec(["sh", "-c", "wc -w /work/notes/today.md"], None).await?;
//!     println!("{}", String::from_utf8_lossy(&result.stdout));
//!     Ok(())
//! }
//! ```
//!
//! ## Structure
//!
//! Two modules, halves that share a crate and one seam.
//!
//! * [`fs`] — expose any path-addressed store as a real filesystem. A store implements
//!   [`FileSystem`](fs::FileSystem) and a binding puts it in front of a concrete interface: a
//!   host FUSE mount, an NFS or FSKit one, whatever else addresses files by path.
//!   `fs/ARCHITECTURE.md` has the long form.
//! * [`console`] — run the commands, wherever the environment is: this host, a micro-VM.
//!   One JSON-RPC channel carries them, and the client is the only end that asks.
//!   `console/ARCHITECTURE.md` has the long form.
//!
//! Nothing in [`console`] builds a tree, calls [`FileSystem`](fs::FileSystem) or
//! touches a binding. What it does take is [`Mount`](fs::Mount)s — trees somebody else
//! already mounted, which a session names as its context and its artifacts, and which a
//! `read` and a command then spell the same file under — and that is the whole of the seam.
//! Two of them because they are two intentions: what the session was given, and what it is
//! to leave behind. Nothing in [`fs`] knows a
//! console exists. There is not even an error type between them: both halves answer in
//! [`std::io::Error`], classified by kind, so neither has a vocabulary the other has to learn.
//!
//! **A console server is built on top of both**, and the crates under
//! `cortex-console-servers/` are what that looks like: `local` runs commands on this host,
//! `uvm/console` runs them in a micro-VM with `uvm/guest` as its far half. Each finds its own
//! way through `fs`, because a tree is mounted by whoever wants one rather than described on
//! the wire and realized at the far end.
//!
//! ## The file layout
//!
//! A module here is a directory whose `mod.rs` holds the module's own documentation and
//! its re-exports, and whose siblings hold the code — including one named after the
//! module itself, for the type the module exists for: `message/message.rs` has
//! [`Message`](console::Message).
//!
//! That is what `module_inception` fires on, and it is the arrangement rather than an
//! accident of one file: the reasoning for a module is long here and belongs somewhere
//! that is not also holding a type, and a reader looking for `Message` should find it in a
//! file with that name.
#![allow(clippy::module_inception)]

pub mod console;
pub mod fs;
pub mod image;
mod lock;
pub mod protocol;

/// What every method that waits hands back — a [`Client`] or a [`Server`].
///
/// Re-exported because implementing either of them means writing the type, and a
/// caller should not have to take a dependency of ours to say what our own traits
/// return. See [`console::base`](console) for why the futures are boxed rather than
/// written as `async fn`.
///
/// [`Client`]: console::Client
/// [`Server`]: console::Server
pub use futures_core::future::BoxFuture;

/// Everything cortex keeps on this host, under one root: `$CORTEX_HOME`, or
/// `cortex` under the user's cache directory.
///
/// **One rule for every kind of thing kept here**, because a host has one answer to "where
/// does this go" and two ways of deriving it are two answers. What hangs off it is split by
/// what owns it -- images below, which every tool in this repository shares, and one
/// directory per live server process, which no other process has any business in -- and that
/// split is a path under the root rather than a root of its own.
///
/// The user's cache directory is `XDG_CACHE_HOME` or `~/.cache`, on macOS
/// `~/Library/Caches`, and on Windows `%LOCALAPPDATA%`, falling back to
/// `%USERPROFILE%\AppData\Local` when that is unset.
pub fn cache_root() -> std::path::PathBuf {
    if let Some(named) = std::env::var_os("CORTEX_HOME") {
        return std::path::PathBuf::from(named);
    }

    #[cfg(windows)]
    return std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("USERPROFILE").unwrap())
                .join("AppData")
                .join("Local")
        })
        .join("cortex");

    #[cfg(target_os = "macos")]
    return std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
        .join("Library")
        .join("Caches")
        .join("cortex");

    #[cfg(not(any(windows, target_os = "macos")))]
    return std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache")
        })
        .join("cortex");
}

/// Fetch the console servers if it is not cached.
pub async fn ensure_cortex() {
    todo!()
}
