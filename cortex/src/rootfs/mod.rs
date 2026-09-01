//! Building a rootfs: a base image, a list of steps, and the image they make.
//!
//! A build is a console session that may [`commit`](crate::console::Console::commit). Each
//! step becomes an `exec` on that session, and the session ends by keeping what it wrote as
//! an image a later session can name. Nothing here knows what an image *is* — that is the
//! console server's, and this module drives a [`Console`](crate::console::Console) and
//! nothing else.

mod dockerfile;
mod id;
mod rootfs;
mod step;

pub use id::BuildId;
pub use rootfs::{BuiltImage, Rootfs, StepFailed, Warning};
pub use step::Step;

pub(crate) use id::{Recipe, digest};
