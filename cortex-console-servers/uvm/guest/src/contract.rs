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
use serde::{Deserialize, Serialize};

/// Where the host wrote this binary in the boot root.
///
/// It is the path libkrun execs, so by the time anything in this crate runs it has already
/// been used — and an ordinary session detaches the root holding it, which is why this end
/// long had no name for it.
///
/// A **committable** session is the exception, and the reason this is here: it keeps that
/// root so a commit can reach the overlay's upper through it, which leaves this binary
/// reachable too. A commit has to leave it out, and cannot leave out what it cannot name.
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
pub const CONTEXT_ENV: &str = "CORTEX_UVM_CONTEXT";

/// Where the session leaves what it produces, spelled the same way. Absent for a session
/// that named none.
pub const ARTIFACTS_ENV: &str = "CORTEX_UVM_ARTIFACTS";

/// Room for the session to work in, spelled the same way — and **where this guest stands**
/// when there is one. Absent for a session that named none.
///
/// The one of the three this end treats differently, which is why the three arrive under
/// three names rather than as one list: mounting them is the same work, and standing in one
/// of them is not.
pub const SCRATCH_ENV: &str = "CORTEX_UVM_SCRATCH";

/// Where `/abin` is, as a device. Absent for a session that has none.
pub const ABIN_ENV: &str = "CORTEX_UVM_ABIN";

/// Where it is mounted, and what goes first on `PATH`.
pub const ABIN_PATH: &str = "/abin";

/// The virtio-fs tag a committable session's scratch directory is shared under.
pub const COMMIT_TAG: &str = "cortexcommit";

/// Where that scratch is mounted, and where a commit writes its layer.
pub const COMMIT_PATH: &str = "/.cortex-commit";

/// Set when there is one to mount. Its value is [`COMMIT_PATH`].
pub const COMMIT_ENV: &str = "CORTEX_UVM_COMMIT";

/// Set when this session may commit at all.
///
/// Read before `pivot_root`, which is why it is separate from [`COMMIT_ENV`]: it decides
/// whether the old root is kept, and that cannot be decided again afterwards.
pub const COMMITTABLE_ENV: &str = "CORTEX_UVM_COMMITTABLE";

/// What a guest answers a `commit` with: the layer is written, and this big.
///
/// Not a [`CommitResp`](cortex::console::CommitResp), which names an image — something
/// only the host can make, and only after reading what this wrote. The console server replaces
/// this with one before the client sees anything, which is what it already does with `init`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GuestCommit {
    /// The tar's size, so the host can tell an empty layer from a missing one.
    pub size: u64,
}

/// What a commit's layer is called inside the scratch. One name, said once.
pub const LAYER_TAR: &str = "layer.tar";

/// The upperdir of the overlay this guest stands on, for a boot that kept its old root.
///
/// The only way a commit sees what a session wrote: the host cannot read that ext4, and the
/// overlay offers its upper under no other name.
pub const UPPER_DIR: &str = "/oldroot/mnt/upper/upper";

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
