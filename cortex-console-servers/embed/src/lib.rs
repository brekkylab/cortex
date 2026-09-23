//! The console servers `cortex` runs, carried as compressed bytes.
//!
//! One constant per enabled feature — [`LOCAL`] for `local`, [`UVM`] for `uvm` — and nothing
//! else. Each is an [`Embedded`]: the server's name, what its bytes hash to, and the bytes
//! themselves as a zstd frame. Where they are written out, and how they are started, is
//! `cortex::console::Backend`'s.
//!
//! The bytes are built by this crate's build script, from the server crates in this
//! workspace, for the target this crate is being built for — see there for how, and for the
//! variables that stand a prebuilt binary in for a build.
#![no_std]

/// One server, as it was built.
#[derive(Debug, Clone, Copy)]
pub struct Embedded {
    /// The program's own name — `cortex-local-console`, `cortex-uvm-console`.
    pub name: &'static str,
    /// The SHA-256 of the **decompressed** program, as lowercase hex.
    ///
    /// Worked out when the bytes were, so that whoever writes them out can name the file by
    /// it without hashing a few megabytes on every start.
    pub digest: &'static str,
    /// How long the program is once decompressed.
    pub len: u64,
    /// The program, as one zstd frame.
    pub zstd: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
