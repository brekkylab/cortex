//! Building a rootfs: a base image, a list of steps, and the image they make.
//!
//! A [`Rootfs`] is a recipe and never more than that. It is handed to a console, and the
//! console runs it if it has to: the recipe's digest names the image it makes, so the
//! `init` that asks for the session is also the question "is this already here?". An image
//! that exists costs one message. One that does not is built first — a session of its own
//! that may [`commit`](crate::console::Console::commit), one `exec` per step, and the
//! commit that keeps what they wrote — and then the session is asked for again.
//!
//! What a build declares is a [`Recipe`] — a base and its steps — and that half is
//! serializable, so it can be stored beside the image it made or sent to whatever will build
//! it. What it is built *against*, the context directory, stays on the `Rootfs`: it is a
//! path on one machine and means nothing on another.
//!
//! Nothing here knows what an image *is*. That is the console server's; this module speaks
//! the protocol and nothing else.
//!
//! ```no_run
//! use cortex::console::Console;
//! use cortex::rootfs::Rootfs;
//!
//! # async fn f() -> anyhow::Result<()> {
//! // A build reaches the internet unless the session says otherwise — see
//! // [`ConsoleBuilder::rootfs`](crate::console::ConsoleBuilder::rootfs), which is where a
//! // recipe meets a server and so the one place a reach is decided.
//! let mut console = Console::builder()
//!     .stdio_client(&["cortex-uvm-console"])
//!     .rootfs(
//!         Rootfs::from_image("python:3.13-slim")
//!             .run("pip install --no-cache-dir pandas")
//!             .env("TZ", "UTC")
//!             .workdir("/srv/app"),
//!     )
//!     .build()
//!     .await?;
//!
//! // And from here the build has stopped mattering: this is an ordinary session.
//! let result = console.exec(["python", "-c", "import pandas"], None).await?;
//! assert_eq!(result.code, 0);
//! # Ok(()) }
//! ```

mod dockerfile;
mod id;
mod recipe;
mod rootfs;
mod step;

pub use id::BuildId;
pub use recipe::Recipe;
pub use rootfs::{Rootfs, StepFailed, Warning};
pub use step::Step;

pub(crate) use id::digest;
pub(crate) use rootfs::{Plan, inside_context, plan, run};
