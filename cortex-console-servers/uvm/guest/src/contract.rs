//! What the host said when it booted this guest.
//!
//! Everything here is a cross-process contract with `cortex-uvm-console`'s `contract`
//! module, which is the end that writes what this end reads. **The two files are one
//! definition in two crates and have to change together** — the host cannot depend on
//! this crate (it is built for a different target) and this crate has no business
//! depending on the host's.
//!
//! The whole contract is a handful of strings, deliberately. A boot passes what it must
//! through the environment because that is the only channel libkrun's `exec` has: an
//! argument vector would work too, but it becomes the guest's kernel command line, which
//! is size-limited and rejects a newline.
//!
//! Nothing that could grow is here. The session — the tree, the commands, the files —
//! arrives on [`PORT_NAME`] as protocol frames, which is what makes this list stay
//! short.
//!
//! Where the host *writes* this binary in the boot root is not here either, and could not
//! usefully be: it is the path libkrun execs, so by the time anything in this crate runs it
//! has already been used, and [`init`](crate::init) detaches the root holding it. The host
//! keeps that one — `cortex-uvm-boot`'s `GUEST_BIN_PATH` — because the host is the only end
//! that needs a name for it.

use serde::{Deserialize, Serialize};

/// The virtio-console port carrying the console session. The host holds a socket at the
/// other end of it; the guest reaches it as `/dev/virtio-ports/<name>`, or by the device
/// whose `name` attribute says so — see [`init::open_port`](crate::init::open_port).
pub const PORT_NAME: &str = "cortex-console";

/// Sent by this end, once, as soon as the port is open, and consumed by the host before
/// it puts a frame on the wire.
///
/// It exists because of one detail of virtio-console: data the host writes to a port no
/// process in the guest has opened is **discarded**, not queued. So the host cannot send
/// `init` when the child connects — it has to wait until something in here is reading,
/// and this is that something saying so.
///
/// Eight bytes and not a frame, because it is the transport coming up rather than
/// anything the protocol has an opinion about.
pub const HANDSHAKE: &[u8; 8] = b"CORTEXUV";

/// The read-only base image, as a guest block device — the overlay's lower.
pub const LOWER_ENV: &str = "CORTEX_UVM_LOWER";

/// The writable session image, as a guest block device — the overlay's upper.
pub const UPPER_ENV: &str = "CORTEX_UVM_UPPER";

/// The workspace share, as `tag:/guest/path`. Absent for a session with no workspace,
/// which is a session.
pub const SHARE_ENV: &str = "CORTEX_UVM_SHARE";

/// `PATH` for everything an execution spawns when the base image did not say — see
/// [`ImageSpec::env`].
///
/// Set here rather than inherited, because there is nothing to inherit it from: libkrun
/// hands the guest's first process the environment the boot named and nothing else, so a
/// guest without this line is one where `sh` cannot find `ls`.
pub const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Where the host left [`ImageSpec`], in the boot root — so it is readable only until the
/// pivot detaches that root, which is why [`init`](crate::init) reads it on the way past.
pub const IMAGE_SPEC_PATH: &str = "/.cortex-image";

/// What the base image says about running a process in it, as the host encoded it at
/// [`IMAGE_SPEC_PATH`].
///
/// The other end is `cortex-uvm-console`'s `contract::ImageSpec`, and **the two files are one
/// definition in two crates**: neither can depend on the other, so a field added on one side
/// and not the other is a silent absence rather than a compile error.
///
/// Its whole job is to stop this end from inventing what the image already stated. An OCI
/// image's `ENV` is where a Debian-based Python image says its `PATH` and its `LANG`, and a
/// guest that ignored it would run commands in an environment the image was never built for.
///
/// Everything here is a *default*: the session's own values win, because a command that
/// arrives with somewhere to be is not asking the image where to stand.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImageSpec {
    /// `KEY=VALUE`, as the image spelled them. Entries with no `=` are not variables and are
    /// dropped by [`agent`](crate::agent).
    pub env: Vec<String>,

    /// Where the image expects a process to stand. Used only by a session with no tree — one
    /// that has a tree stands in it, which is the whole reason it named one.
    pub working_dir: Option<String>,
}

/// The interface the boot attached, as `iface=<name>,mac=<addr>,mtu=<n>`. Absent for a guest
/// with no network, which is the default.
///
/// **Not this project's spelling.** These are `microsandbox-network`'s own variable names, and
/// they arrive from the stack that assigned the values — passed through by the boot rather than
/// translated, so there is one description of the network and not two. Its guest agent reads
/// the same names to do the same job, which is what makes this the shape to match rather than
/// invent.
///
/// The `mac=` field is the one that is not description: it is how [`net`](crate::net) tells the
/// attached interface from the others the kernel brought up on its own.
pub const NET_ENV: &str = "MSB_NET";

/// The addresses on that interface, as `addr=<ip>/<prefix>,gw=<ip>,dns=<ip>`.
///
/// A separate variable from [`NET_ENV`] because a stack with no IPv4 route to offer sends no
/// IPv4 — and one that has both sends `MSB_NET_IPV6` beside it, which this end does not read
/// yet.
pub const NET_IPV4_ENV: &str = "MSB_NET_IPV4";

/// Where a resolver is named on any system a base image was built for.
pub const RESOLV_CONF: &str = "/etc/resolv.conf";
