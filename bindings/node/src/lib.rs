//! Node bindings for cortex, built with napi-rs.
//!
//! The shape is cortex's own, spelled in JavaScript: a [`Console`](cortex::console::Console)
//! is built by a builder and awaited, a [`Directory`](cortex::fs::Directory) is assembled
//! and handed to a host mount, and an [`Image`](cortex::image::Image) is a base and its
//! steps. Names are camelCased and nothing else changes — a caller reading cortex's Rust
//! documentation should find the same names doing the same things.
//!
//! One module per half, as in the crate: [`console`] for running commands, [`fs`] for the
//! trees they see, [`image`] for what they run on, and [`error`] for how either half's
//! failures arrive as JavaScript errors.

mod console;
mod error;
mod fs;
mod image;
