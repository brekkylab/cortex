//! # Cortex
//!
//! The environment an agent works in: what it can see, and what it can do.
//!
//! Both are ordinary on purpose: an agent handed a filesystem and a shell already knows both
//! interfaces, and every added capability arrives as a file to read or a command to run.
//!
//! * **What it sees is a filesystem.** Anything it should know about (a project directory, an
//!   object store, a Notion workspace, an in-memory tree) implements
//!   [`FileSystem`](fs::FileSystem). A [`Directory`](fs::Directory) is the one a caller
//!   assembles: in-memory files plus host directories grafted in at chosen paths. A binding
//!   mounts it on the host, so it is read with `cat`, `grep`, or anything else.
//! * **What it does is run commands.** A [`ConsoleClient`](console::ConsoleClient) runs them on
//!   this host or in a micro-VM over one channel: an `exec` carries an argv, and everything the
//!   command wrote comes back.
//!
//! ## Quickstart
//!
//! Needs the `mount` feature. On Linux the binding is `FuseMount`; macOS uses `FuseTMount` and
//! Windows `DokanMount`, with nothing else changed.
//!
//! ```ignore
//! use std::path::Path;
//!
//! use cortex::console::ConsoleClient;
//! use cortex::fs::{Directory, FuseMount};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // What the agent sees: an in-memory file beside a host directory.
//!     let context = Directory::new()
//!         .with_file("notes/today.md", "ship the release".as_bytes())?
//!         .with_mount("project", "/home/me/project")?;
//!
//!     // Constructing the guard mounts, dropping it unmounts; the console holds it for the session.
//!     let mount = FuseMount::try_new(context, Path::new("/tmp/session"))?;
//!
//!     // What the agent can do. A session's shape, including where each tree appears to
//!     // commands, is fixed when the console is built.
//!     let mut console = ConsoleClient::builder()
//!         .mount(mount, "/work")
//!         .build()
//!         .await?;
//!
//!     let result = console.exec(["sh", "-c", "wc -w /work/notes/today.md"], None).await?;
//!     println!("{}", String::from_utf8_lossy(&result.stdout));
//!     Ok(())
//! }
//! ```
//!
//! ## Structure
//!
//! Two modules sharing one seam.
//!
//! * [`fs`] — expose any path-addressed store as a real filesystem. A store implements
//!   [`FileSystem`](fs::FileSystem) and a binding puts it behind a concrete interface (host
//!   FUSE, NFS, FSKit, anything addressing files by path). See `fs/ARCHITECTURE.md`.
//! * [`console`] — run the commands on this host or in a micro-VM, over one JSON-RPC channel
//!   where only the client asks. See `console/ARCHITECTURE.md`.
//!
//! [`console`] never builds a tree, calls [`FileSystem`](fs::FileSystem) or touches a binding;
//! it only takes [`Mount`](fs::Mount)s, trees already mounted, which a session names as its
//! context and its artifacts (what it was given and what it leaves behind), and under which a
//! `read` and a command spell the same file. [`fs`] does not know a console exists. Both answer
//! in [`std::io::Error`], classified by kind, so there is no shared error type.
//!
//! **A console server is built on both**; see `cortex-console-servers/`: `local` runs commands
//! on this host, `uvm/console` in a micro-VM with `uvm/guest` as its far half. Each mounts
//! through `fs` itself, since a tree is mounted by whoever wants it rather than described on
//! the wire.
//!
//! ## The file layout
//!
//! A module is a directory whose `mod.rs` holds its documentation and re-exports, and whose
//! siblings hold the code, including one named after the module for its main type:
//! `message/message.rs` has [`Message`](protocol::Message).
//!
//! `module_inception` is allowed deliberately: long module docs belong apart from any type,
//! and a reader looking for `Message` should find it in a file of that name.
#![allow(clippy::module_inception)]

pub mod console;
#[cfg(feature = "ensure")]
mod ensure;
pub mod fs;
pub mod image;
mod lock;
pub mod protocol;

/// What every waiting method of a [`Client`] or [`Server`] returns.
///
/// Re-exported so implementors can name it without depending on `futures_core`. See
/// [`console::base`](console) for why futures are boxed rather than `async fn`.
///
/// [`Client`]: console::Client
/// [`Server`]: console::Server
pub use futures_core::future::BoxFuture;

#[cfg(feature = "ensure")]
pub use ensure::ensure_cortex;

/// Everything cortex keeps on this host, under one root: `$CORTEX_HOME`, or
/// `cortex` under the user's cache directory.
///
/// **One root for everything kept here**, so a host has a single answer to "where does this
/// go". Contents are split by owner as paths under it, not separate roots: images, shared by
/// every tool in this repository, and one directory per live server process, private to it.
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
