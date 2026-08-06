pub mod console;
mod error;
pub mod executable;
pub mod volume;

pub use error::*;

/// What every method that waits hands back — a [`Client`], a [`Server`], an
/// [`Executable`].
///
/// Re-exported because implementing any of those three means writing the type, and a
/// caller should not have to take a dependency of ours to say what our own traits
/// return. See [`console::base`](console) for why the futures are boxed rather than
/// written as `async fn`.
///
/// [`Client`]: console::Client
/// [`Server`]: console::Server
/// [`Executable`]: executable::Executable
pub use futures_core::future::BoxFuture;
