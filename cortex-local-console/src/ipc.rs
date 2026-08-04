//! How a shim finds its way back to whoever can answer for the name it stands in for.
//!
//! Both roles need this, which is why it is here rather than inside either of them: the
//! server binds the socket and names it in the environment of everything an execution
//! spawns, and the shim reads the path back out.
//!
//! # The server binds it, and passes the call along
//!
//! A delegated name's behaviour lives in the client's
//! [`ExecutableSet`](cortex::executable::ExecutableSet), so the client is what has to
//! answer. But the shim does not reach it directly. It reaches *us*, and we hand the call
//! to the client as a [`Delegated`](cortex::console::Progress::Delegated) — a response on
//! the console channel, on the request the client is already waiting on — then hand back
//! whatever the client resumes with.
//!
//! Which is what keeps every channel one-directional. The alternative was for the client
//! to bind this socket and be dialled directly, and it worked; what it cost was a client
//! that had to be an answering end as well as an asking one — a listener, a thread per
//! delegated call, and a socket path that only the client could choose and only the
//! server's environment could carry.
//!
//! Going through the server also puts the hop where the processes are. A shim and the
//! command that ran it are on the same side of whatever boundary the console channel
//! crosses, so a unix socket is enough for both — where reaching the client meant reaching
//! across it, which for a micro-VM guest a unix socket cannot do.
//!
//! The wire on it is [`read`](cortex::console::stdio::read) and
//! [`write`](cortex::console::stdio::write) — the same framing and the same
//! [`Message`](cortex::console::Message) the console channel carries, so there is one
//! codec in the system rather than two, and a shim's call is an `exec` like any other.
//!
//! A unix socket rather than the shim's own stdio: its stdio belongs to whatever ran it,
//! which may be a pipeline, and is the only place its output can go.

/// Where the shim finds the socket. Put into every execution's environment by the server,
/// so anything spawned under it — including a shell, and anything that shell spawns —
/// inherits the way home.
pub const SOCK_ENV: &str = "CORTEX_CONSOLE_SOCK";
