//! # Virtx
//!
//! The environment an agent works in: what it can see, and what it can do.
//!
//! Both are ordinary on purpose: an agent handed a filesystem and a shell already knows both
//! interfaces, and every added capability arrives as a file to read or a command to run.
//!
//! * **What it sees is a filesystem**, read with `cat`, `grep` or anything else once a
//!   binding mounts it on the host.
//! * **What it does is run commands**, each an argv sent over one channel, with everything the
//!   command wrote coming back.
//!
//! ## Quickstart
//!
//! Needs the `mount` feature. On Linux the binding is `FuseMount`; macOS uses `FuseTMount` and
//! Windows `DokanMount`, with nothing else changed.
//!
//! ```ignore
//! use std::path::Path;
//!
//! use virtx::console::ConsoleClient;
//! use virtx::fs::{Directory, FuseMount};
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
//! * [`fs`]: what the agent sees. Stores, the in-memory [`Directory`](fs::Directory), and the
//!   bindings that mount them on the host.
//! * [`console`]: what the agent does. [`ConsoleClient`](console::ConsoleClient) runs commands
//!   in a session.
//! * [`protocol`]: the wire between a console client and a console server. The server lives in
//!   its own repository (virtx-uvm).
//! * [`image`]: the images a session runs on, declared or built ahead of time.
//!
//! [`fs`] and [`console`] meet only at [`Mount`](fs::Mount): a console takes trees already
//! mounted and never builds one or touches a binding, and [`fs`] does not know a console
//! exists. There is no shared error type: [`fs`] answers in [`std::io::Error`], classified by
//! kind, and [`console`] in [`Failure`](protocol::Failure), or [`anyhow::Error`] while building.
//!
//! ## The file layout
//!
//! A module with submodules is a directory whose `mod.rs` holds its documentation and
//! re-exports, and whose siblings hold the code, including one named after the module for its
//! main type (`message/message.rs` has [`Message`](protocol::Message)); hence
//! `module_inception` is allowed.
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
/// Re-exported so implementors can name it without depending on `futures_core`.
///
/// [`Client`]: protocol::Client
/// [`Server`]: protocol::Server
pub use futures_core::future::BoxFuture;

#[cfg(feature = "ensure")]
pub use ensure::ensure_virtx;

/// Everything virtx keeps on this host, under one root: `$VIRTX_HOME`, or
/// `virtx` under the user's cache directory.
///
/// One root, so a host has a single answer to "where does this go": contents are split by
/// owner as paths under it. This crate keeps only `bin/`, where the console server lives.
///
/// The user's cache directory is `XDG_CACHE_HOME` or `~/.cache`, on macOS
/// `~/Library/Caches`, and on Windows `%LOCALAPPDATA%`, falling back to
/// `%USERPROFILE%\AppData\Local` when that is unset.
pub fn cache_root() -> std::path::PathBuf {
    if let Some(named) = std::env::var_os("VIRTX_HOME") {
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
        .join("virtx");

    #[cfg(target_os = "macos")]
    return std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
        .join("Library")
        .join("Caches")
        .join("virtx");

    #[cfg(not(any(windows, target_os = "macos")))]
    return std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache")
        })
        .join("virtx");
}
