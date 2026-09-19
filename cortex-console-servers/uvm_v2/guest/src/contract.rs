//! What a boot tells the guest.
//!
//! The other end of this is the guest's own reading of it, which **has to change with this
//! file** — that half is built for a different target and takes nothing it does not need, so
//! the compiler will not notice a mismatch.
//!
//! Where a tree lands inside the guest is **this crate's constant for it** —
//! [`CONTEXT_PATH`], [`ARTIFACTS_PATH`], [`SCRATCH_PATH`] — carried in that tree's env
//! alongside the tag. One name per role, the same in every session, so what a guest reports
//! and what a client sends are the same three strings no matter which host directory is
//! behind them.
//!
//! Three of them, one per tree a session can name, and three envs rather than one list
//! because the guest does something different with one of them: it **stands** in the scratch.
//! A list would carry the same three strings and leave the guest to work out which was which.
//!
//! The **guest** half is environment because that is the only channel libkrun's `exec` has:
//! an argv and an env value alike become part of the guest's kernel command line, which is
//! size-limited and rejects a newline. Nothing that could grow is there — the session itself
//! arrives on [`PORT_NAME`] as protocol frames, and what the base image said arrives as a file
//! ([`IMAGE_SPEC_PATH`]).

// The whole contract, whether or not this half reads every part of it.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Where a boot writes the guest binary in the boot root, and therefore the path libkrun
/// execs.
///
/// It names a file on the boot root and nothing else: the guest pivots onto its own root and
/// detaches this one, so the path stops existing the moment there is a guest, and nothing on
/// either side looks for it again.
pub const GUEST_BIN_PATH: &str = "/.cortex-guest";

/// The virtio-console port carrying the console session.
pub const PORT_NAME: &str = "cortex-console";

/// Sent by the guest, once, as soon as it has the port open, and consumed here before the
/// first frame goes out.
///
/// A virtio-console port **discards** what the host writes while no process in the guest
/// has it open, rather than queueing it. So a boot cannot put `init` on the wire as soon
/// as the child connects — the socket is up long before the kernel is — and this is the
/// guest saying that something is reading.
pub const HANDSHAKE: &[u8; 8] = b"CORTEXUV";

/// The overlay's lower, as the guest sees it. Attach order is what fixes the names, so
/// this is a promise `cortex-uvm-boot` keeps rather than something either end computes.
pub const GUEST_LOWER_DEV: &str = "/dev/vdb";

/// The read-only `/abin` disk, when a session has one.
///
/// Third, because the session's own image is `/dev/vda` and the base is `/dev/vdb` — the
/// order a boot attaches disks in is the order the guest names them.
pub const GUEST_ABIN_DEV: &str = "/dev/vdc";

/// The overlay's upper, as the guest sees it.
pub const GUEST_UPPER_DEV: &str = "/dev/vda";

/// Told to the guest as `CORTEX_UVM_LOWER`, which is the name its `contract` module reads.
pub const LOWER_ENV: &str = "CORTEX_UVM_LOWER";

/// Told to the guest as `CORTEX_UVM_UPPER`.
pub const UPPER_ENV: &str = "CORTEX_UVM_UPPER";

/// Told to the guest as `CORTEX_UVM_CONTEXT`, spelled `tag:/guest/path` — the tag is
/// [`CONTEXT_TAG`] and the path is [`CONTEXT_PATH`].
pub const CONTEXT_ENV: &str = "CORTEX_UVM_CONTEXT";

/// The artifacts tree, spelled the same way. Absent for a session that named none.
pub const ARTIFACTS_ENV: &str = "CORTEX_UVM_ARTIFACTS";

/// The scratch tree, spelled the same way, and **where the guest stands** when there is one.
pub const SCRATCH_ENV: &str = "CORTEX_UVM_SCRATCH";

/// Where the context is mounted inside the guest.
///
/// **A name for the role and not for the directory behind it.** What a session is given is a
/// directory somewhere on the host, and what it is *called* in here says which of the three
/// trees it is — so a command reads `/context/src/main.rs` and a client sends the same
/// string, in a session that would have spelled it `/Users/someone/project/src/main.rs` on
/// the other side of the hypervisor and in another one that would have spelled it `/srv/wt/9`.
///
/// Three consequences, and all three are the reason:
///
/// - **A guest's paths do not name the host.** Where a caller keeps its projects is that
///   caller's, and a path is the easiest thing in the world to leak — into a command's
///   output, a log, a build artifact, a model's transcript.
/// - **Two sessions over different directories are the same session to look at**, which is
///   what makes a transcript comparable and a command reproducible.
/// - **The guest's root stays a root.** Mounting at the host's own path means a guest with
///   `/Users/someone/project` in it, which is a directory tree invented inside an image to
///   mirror a stranger's laptop.
///
/// What it costs is that a mount point has two names, one per side — and nothing has to
/// translate between them, because the protocol only ever speaks this one. The host answers
/// `init` with it, the replayed `init` names it, the guest stands in it and reports it, and a
/// `read`'s path arrives already spelled the way the guest has it. The other name is the
/// caller's own [`Mount`](https://docs.rs/cortex/latest/cortex/fs/trait.Mount.html), which is
/// the thing that put the directory there and never needed the protocol to tell it where.
pub const CONTEXT_PATH: &str = "/context";

/// Where the artifacts tree is mounted inside the guest — see [`CONTEXT_PATH`].
pub const ARTIFACTS_PATH: &str = "/artifacts";

/// Where the scratch tree is mounted inside the guest, and where a session stands when it has
/// one — see [`CONTEXT_PATH`].
pub const SCRATCH_PATH: &str = "/scratch";

/// Where the guest is told to find `/abin`, as a device. Absent for a session that has none,
/// which is a guest with no `/abin` at all rather than an empty one.
pub const ABIN_ENV: &str = "CORTEX_UVM_ABIN";

/// The virtio-fs tag the context is attached under. Never seen by a caller: it is an
/// identifier two device configurations agree on, and the guest mounts it by this name.
pub const CONTEXT_TAG: &str = "cortexctx";

/// The tag the artifacts tree is attached under.
pub const ARTIFACTS_TAG: &str = "cortexart";

/// The tag the scratch tree is attached under.
///
/// One device per tree rather than one device with three directories under it, because the
/// host has three directories and no common parent to serve: each is somewhere the caller
/// mounted it, and inventing a parent would mean the host arranging its own filesystem around
/// what this protocol happens to carry.
pub const SCRATCH_TAG: &str = "cortexscratch";

/// The virtio-fs tag a committable session's scratch directory is shared under.
pub const COMMIT_TAG: &str = "cortexcommit";

/// Where the guest finds that scratch, and where it writes a commit's layer.
///
/// Not one of the three above and not reached the way they are: nothing outside this
/// workspace names it, so it is a path the two halves of the commit agree on and no client
/// ever sees.
pub const COMMIT_PATH: &str = "/.cortex-commit";

/// Set for a guest that has one. Its value is [`COMMIT_PATH`].
pub const COMMIT_ENV: &str = "CORTEX_UVM_COMMIT";

/// Set for a guest that may commit at all.
///
/// Separate from [`COMMIT_ENV`] because the two are read at different moments and decide
/// different things: this one before `pivot_root`, to keep the old root rather than detach it,
/// and that one after, to mount somewhere to write. A guest with the share and no kept root
/// would have somewhere to put a layer and no way to see one.
pub const COMMITTABLE_ENV: &str = "CORTEX_UVM_COMMITTABLE";

/// What a commit's layer is called inside the scratch. One name, said once.
pub const LAYER_TAR: &str = "layer.tar";

/// Where a guest that kept its old root can reach the upperdir of the overlay it stands on.
///
/// The host cannot read that ext4 itself and the overlay offers the upper under no other
/// name, so this is the only way a commit sees what a session wrote.
pub const UPPER_DIR: &str = "/oldroot/mnt/upper/upper";

/// before the pivot detaches that root.
pub const IMAGE_SPEC_PATH: &str = "/.cortex-image";

/// What the base image says about running a process in it, as BSON at [`IMAGE_SPEC_PATH`].
///
/// The far end is `cortex-uvm-guest`'s `contract::ImageSpec`, which **has to change with
/// this one** — the two crates are built for different targets, so the compiler cannot see a
/// mismatch. BSON because it is the codec both ends already carry for the console wire.
///
/// Two fields of an OCI config, and deliberately not the rest. `Entrypoint` and `Cmd` have
/// nobody to instruct: a command's argv comes from the client. `ExposedPorts` and `Volumes`
/// describe things this backend does not have. `User` is the one left out on purpose rather
/// than for want of a use — running as the image's user changes who owns writes to a tree
/// shared over virtio-fs, and that is a decision with a failure mode too quiet to make as a
/// side effect of reading a config.
///
/// A base with no config — the rootfs tarball — gets the default, which says nothing and
/// leaves every fallback in place.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImageSpec {
    /// `KEY=VALUE`, as the image spelled them.
    pub env: Vec<String>,

    /// Where the image expects a process to stand, if it said so.
    pub working_dir: Option<String>,
}

/// How much of the network a session may reach.
///
/// The addresses are deliberately absent. A boot's network is a userspace stack running in the
/// boot process, which assigns them itself and tells the guest what they are directly — as the
/// `MSB_NET*` variables it puts in the guest's exec environment. So there is nothing here for
/// the two ends to agree on: this crate carries the *decision*, and the stack carries the
/// numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    /// No device at all. A guest with nothing attached reaches nothing, which is a stronger
    /// statement than any policy over a device that is there.
    Disabled,

    /// A device, the gateway's resolver, and whatever [`HOST_PORTS`] granted. Nothing else.
    ///
    /// The default, and the reason there is a default at all: names resolve, so a command that
    /// tries to reach the internet fails at a refused connection rather than at a lookup that
    /// hangs. A caller who wants egress asks for it.
    ///
    /// What makes it a reach rather than an error message is the ports: a session granted one
    /// talks to whatever the operator put on the host — a model proxy, a registry cache, a
    /// sidecar — without any of it being on the internet.
    ///
    /// **Not an air gap.** The stack forwards lookups to the host's own resolver, so a name is a
    /// way out for anything small enough to spell. [`Disabled`](Self::Disabled) is the air gap.
    #[default]
    HostOnly,

    /// [`HostOnly`](Self::HostOnly) plus the public internet. Private ranges, link-local
    /// addresses and the host's own loopback stay refused: a sandbox that can reach a LAN is
    /// reaching things nobody published to it.
    ///
    /// Reaching the host is still only what [`HOST_PORTS`] granted, which is the point of that
    /// being a separate grant: **widening the outside must never widen the inside.**
    Public,

    /// Everything, with no policy. For a caller who has decided the sandbox boundary is
    /// somewhere else.
    Full,
}

impl Network {
    pub fn as_str(self) -> &'static str {
        match self {
            Network::Disabled => "none",
            Network::HostOnly => "host",
            Network::Public => "public",
            Network::Full => "full",
        }
    }

    /// Parse the name a server wrote, where absent and empty both mean the default.
    ///
    /// An unrecognised value is refused. `on`, `true` and `1` are all things a caller might
    /// reasonably type and none of them says how much reach is wanted, so guessing at one would
    /// be deciding a sandbox's egress on their behalf.
    pub fn parse(value: Option<&str>) -> anyhow::Result<Network> {
        match value {
            None | Some("") => Ok(Network::default()),
            Some("none") => Ok(Network::Disabled),
            Some("host") => Ok(Network::HostOnly),
            Some("public") => Ok(Network::Public),
            Some("full") => Ok(Network::Full),
            Some(other) => {
                anyhow::bail!("--network: {other} is not `none`, `host`, `public` or `full`")
            }
        }
    }
}

/// Where it is mounted, and what goes first on `PATH`.
pub const ABIN_PATH: &str = "/abin";

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

/// `PATH` for everything an execution spawns when the base image did not say — see
/// [`ImageSpec::env`].
///
/// Set here rather than inherited, because there is nothing to inherit it from: libkrun
/// hands the guest's first process the environment the boot named and nothing else, so a
/// guest without this line is one where `sh` cannot find `ls`.
pub const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

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
