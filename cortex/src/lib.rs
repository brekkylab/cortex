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
//!   [`FileSystem`](fs::FileSystem) and is grafted into a [`ContextFs`](fs::ContextFs) at a path
//!   the caller chooses. A binding mounts that tree on the host, so what reads it is `cat`,
//!   `grep`, and whatever else the agent thought to run.
//! * **What it does is run commands.** A [`Console`](console::Console) runs them somewhere —
//!   this host, a micro-VM — over one channel: an `exec` carrying an argv, and everything the
//!   command wrote coming back.
//!
//! ## Quickstart
//!
//! Needs the `fuse` feature, which is where `FuseMount` comes from — under `fuse-t` the same
//! two lines say `FuseTMount`, and nothing else about this changes.
//!
//! ```ignore
//! use std::path::Path;
//!
//! use cortex::console::Console;
//! use cortex::fs::{ContextFs, FuseMount, InMemFs};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // What the agent can see. Each store is a `FileSystem`; a `ContextFs` is several of
//!     // them under one root, and is itself a `FileSystem`, so a binding drives it like
//!     // any single store.
//!     let context = ContextFs::new().try_with_mount("notes", InMemFs::new())?;
//!
//!     // Where the host can see it. Constructing the guard mounts, dropping it unmounts,
//!     // and the console below holds it for the length of the session.
//!     let mount = FuseMount::try_new(context, Path::new("/tmp/session"))?;
//!
//!     // What the agent can do: a server that runs its commands, against that tree. A
//!     // session's shape is said once, when the console is built — including where each
//!     // tree appears to the commands, which is what their paths are spelled under.
//!     let mut console = Console::builder()
//!         .stdio_client(&["cortex-local-console"])
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
//! module itself, for the type the module exists for: `console/console.rs` has
//! [`Console`](console::Console), `message/message.rs` has
//! [`Message`](console::Message).
//!
//! That is what `module_inception` fires on, and it is the arrangement rather than an
//! accident of one file: the reasoning for a module is long here and belongs somewhere
//! that is not also holding a type, and a reader looking for `Console` should find it in
//! a file with that name.
#![allow(clippy::module_inception)]

pub mod console;
pub mod fs;
mod lock;
pub mod rootfs;

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
