//! Executables the crate ships, as opposed to the ones a consumer writes.
//!
//! [`Executable`](crate::exec::Executable) exists so that behaviour can live wherever the client
//! does, and most implementations therefore belong to whoever registers them — `cortex-execs/`
//! is where those are. What is here is the one implementation that is not about any particular
//! behaviour: [`BinExecutable`] runs a program that already exists on this host, so the
//! *behaviour* is the program's and what this crate contributes is the translation.
//!
//! That translation is the reason it is not a consumer's to write. A delegated call arrives with
//! a working directory and an environment belonging to another machine, and turning those into
//! something a host process can be given correctly is the same work every time: which variables
//! may cross, which this host has to answer itself, what a program with no tree to stand in is
//! told, and what happens to a program that will not end.

mod bin;

pub use bin::BinExecutable;
