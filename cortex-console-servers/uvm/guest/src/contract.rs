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
//! Nothing that could grow is here. The session — the delegated names, the commands, the
//! files — arrives on [`PORT_NAME`] as protocol frames, which is what makes this list
//! stay short.

/// Where the host writes this binary in the boot root, and therefore the path libkrun
/// execs. Also where [`init`](crate::init) puts a copy of it on the new root after the
/// pivot, so the shim symlinks have a file to point at.
pub const GUEST_BIN_PATH: &str = "/.cortex-guest";

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

/// `PATH` for everything an execution spawns, before the delegated names are appended.
///
/// Set here rather than inherited, because there is nothing to inherit it from: libkrun
/// hands the guest's first process the environment the boot named and nothing else, so a
/// guest without this line is one where `sh` cannot find `ls`.
pub const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
