//! The half of `cortex-uvm-console` that is not the server: layers, and the disk several of
//! them make.
//!
//! It is a library because a test has to be able to *build* an image before it can ask a
//! console to boot one, and because the same store is what `/abin` and a build's `commit`
//! will both put their layers in. The server itself stays in the binary — what a console
//! does with a session is answered over a channel, not called.

pub mod built;
pub mod layer;
