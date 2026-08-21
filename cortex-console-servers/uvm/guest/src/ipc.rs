//! How a shim finds its way back to whoever can answer for the name it stands in for.
//!
//! Both roles need this, which is why it is here rather than inside either of them: the
//! agent binds the socket and names it in the environment of everything an execution
//! spawns, and the shim reads the path back out.
//!
//! # The hop is guest-local, and that is the point
//!
//! A delegated name's behaviour lives in the client's
//! [`ExecutableSet`](cortex::exec::ExecutableSet), which is on the host, on the far
//! side of a hypervisor. A shim cannot reach it: a unix socket does not cross that
//! boundary, and giving the guest a way to dial the host would mean a second channel with
//! its own addressing, its own lifetime and its own failure modes.
//!
//! So the shim reaches the **agent**, which is a process in the same guest, and the agent
//! passes the call on as a [`Delegated`](cortex::console::Progress::Delegated) — a
//! response on the console request the client is already waiting on. The console channel
//! already crosses the hypervisor; nothing else has to.
//!
//! That is the same shape [`cortex-local-console`] uses, and it is not a coincidence: the
//! reason it is right there — a shim and the command that ran it are on the same side of
//! whatever boundary the console channel crosses — is the reason it is right here, only
//! with a boundary worth naming.
//!
//! The wire on it is [`read`](cortex::console::stdio::read) and
//! [`write`](cortex::console::stdio::write) — the same framing and the same
//! [`Message`](cortex::console::Message) the console channel carries, so there is one
//! codec in the system rather than two, and a shim's call is an `exec` like any other.
//!
//! A unix socket rather than the shim's own stdio: its stdio belongs to whatever ran it,
//! which may be a pipeline, and is the only place its output can go.
//!
//! [`cortex-local-console`]: https://docs.rs/cortex-local-console

/// Where the shim finds the socket. Put into every execution's environment by the agent,
/// so anything spawned under it — including a shell, and anything that shell spawns —
/// inherits the way home.
pub const SOCK_ENV: &str = "CORTEX_CONSOLE_SOCK";
