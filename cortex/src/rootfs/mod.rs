//! Building a rootfs: a base image, a list of steps, and the image they make.
//!
//! A build is a console session that may [`commit`](crate::console::Console::commit). Each
//! step becomes an `exec` on that session, and the session ends by keeping what it wrote as
//! an image a later session can name. Nothing here knows what an image *is* — that is the
//! console server's, and this module drives a [`Console`](crate::console::Console) and
//! nothing else.

mod id;
mod step;

pub use id::BuildId;
pub use step::Step;

#[allow(unused_imports)]
pub(crate) use id::{Recipe, digest};
