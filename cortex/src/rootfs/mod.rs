//! Building a rootfs: a base image, a list of steps, and the image they make.
//!
//! A build is a console session that may [`commit`](crate::console::Console::commit). Each
//! step becomes an `exec` on that session, and the session ends by keeping what it wrote as
//! an image a later session can name. Nothing here knows what an image *is* — that is the
//! console server's, and this module drives a [`Console`](crate::console::Console) and
//! nothing else.
//!
//! ```no_run
//! use cortex::console::{Console, ConsoleBuilder};
//! use cortex::rootfs::Rootfs;
//!
//! # async fn f() -> anyhow::Result<()> {
//! // A build reaches the internet unless it says otherwise — see [`Rootfs::network`],
//! // which is also how to take that away.
//! let built = Rootfs::from_image("python:3.13-slim")
//!     .run("pip install --no-cache-dir pandas")
//!     .env("TZ", "UTC")
//!     .workdir("/srv/app")
//!     .build(|| ConsoleBuilder::new().stdio_client(&["cortex-uvm-console"]))
//!     .await?;
//!
//! // And a session on what it made, which is where the build stops mattering.
//! let console = Console::builder()
//!     .stdio_client(&["cortex-uvm-console"])
//!     .image(&built)
//!     .build()
//!     .await?;
//! # Ok(()) }
//! ```

mod dockerfile;
mod id;
mod rootfs;
mod step;

pub use id::BuildId;
pub use rootfs::{BuiltImage, Rootfs, StepFailed, Warning};
pub use step::Step;

pub(crate) use id::{Recipe, digest};
pub(crate) use rootfs::inside_context;
