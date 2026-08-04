//! How a shim finds its way back to whoever owns the behaviour it stands in for.
//!
//! Both roles need this, which is why it is here rather than inside either of them: the
//! shim reads the path, and the server role passes it on by doing nothing — it is
//! already in the environment it was started with, so everything it spawns inherits it.
//!
//! # The client binds it, not the server
//!
//! A delegated name's behaviour lives in the client's
//! [`ExecutableSet`](cortex::executable::ExecutableSet), so the client is what a shim
//! has to reach. It could reach the server first and have the call passed along, but
//! that would make the server a requester on its own console channel — and each end of
//! that channel does one job, so it cannot be. The shim goes straight to the client.
//!
//! So the **client** binds the socket and puts its path in the environment it spawns the
//! server with. Nothing about the socket appears in the console protocol.
//!
//! The wire on it is [`read`](cortex::console::stdio::read) and
//! [`write`](cortex::console::stdio::write) — the same framing and the same
//! [`Message`](cortex::console::Message) the console channel carries, so there is one
//! codec in the system rather than two, and a shim's call is an `exec` like any other.
//!
//! A unix socket rather than the shim's own stdio: its stdio belongs to whatever ran it,
//! which may be a pipeline, and is the only place its output can go. It is
//! backend-specific for the same reason a micro-VM cannot use one — there a shim reaches
//! the client over a virtio port instead.

/// Where the shim finds the socket. Exported by the client into the server's
/// environment, so anything spawned under it — including a shell, and anything that
/// shell spawns — inherits the way home.
pub const SOCK_ENV: &str = "CORTEX_CONSOLE_SOCK";
