//! What a host tells a boot, and what a boot tells the guest.
//!
//! Two cross-process contracts in one module because three binaries speak them and every one
//! of them takes this crate: `cortex-uvm-v2-host` writes what `cortex-uvm-v2-boot` parses, and
//! the boot in turn writes what `cortex-uvm-v2-guest` reads. The three are built for two
//! targets by three cargos and agree on nothing else, so a name that only they share is a name
//! nothing checks — and a mismatch fails at a mount or a parse, seconds and a kernel log away
//! from the change that caused it. One definition of each, in a library all three link, is what
//! turns that into a compile error.
//!
//! The two are not the same kind of thing:
//!
//! - **the host to the boot** ([`BootArgs`]). An ordinary `Command::spawn`, so it is the
//!   boot's command line. One type with one [`to_args`](BootArgs::to_args) and one
//!   [`parse`](BootArgs::parse), so a value added to the writing side is a value the reading
//!   side stops round-tripping without. It is also the whole of what they say to each other:
//!   a boot is started, told, and never asked anything, which is what lets it be a separate
//!   binary rather than a mode of the host's.
//! - **the boot to the guest** ([`LOWER_ENV`], [`UPPER_ENV`], [`CONTEXT_ENV`],
//!   [`ARTIFACTS_ENV`], [`GUEST_BIN_PATH`], [`PORT_NAME`], [`HANDSHAKE`]).
//!
//! Where a tree lands inside the guest is **this module's constant for it** —
//! [`CONTEXT_PATH`], [`ARTIFACTS_PATH`] — carried in that tree's env alongside the tag. One
//! name per role, the same in every session, so what a guest reports and what a client sends
//! are the same strings no matter which host directory is behind them.
//!
//! One env per tree rather than one list of them, because a name that says which tree it is
//! is a name the guest can act on: the context is mounted read-only and the artifacts tree is
//! not. A list would carry the same paths and leave the guest to work out which was which.
//!
//! The **guest** half is environment because that is the only channel libkrun's `exec` has:
//! an argv and an env value alike become part of the guest's kernel command line, which is
//! size-limited and rejects a newline. Nothing that could grow is there — the session itself
//! arrives on [`PORT_NAME`] as protocol frames, and what the base image said arrives as a file
//! ([`IMAGE_SPEC_PATH`]). None of that constrains [`BootArgs`], which is an ordinary spawn.

use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

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

/// Told to the guest as `CORTEX_UVM_LOWER`.
pub const LOWER_ENV: &str = "CORTEX_UVM_LOWER";

/// Told to the guest as `CORTEX_UVM_UPPER`.
pub const UPPER_ENV: &str = "CORTEX_UVM_UPPER";

/// Told to the guest as `CORTEX_UVM_CONTEXT`, spelled `tag:/guest/path` — the tag is
/// [`CONTEXT_TAG`] and the path is [`CONTEXT_PATH`].
pub const CONTEXT_ENV: &str = "CORTEX_UVM_CONTEXT";

/// The artifacts tree, spelled the same way. Absent for a session that named none.
pub const ARTIFACTS_ENV: &str = "CORTEX_UVM_ARTIFACTS";

/// Where the context is mounted inside the guest.
///
/// **A name for the role and not for the directory behind it.** What a session is given is a
/// directory somewhere on the host, and what it is *called* in here says which of the two
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

/// Where the guest is told to find `/abin`. Absent for a session that has none, which is a
/// guest with no `/abin` at all rather than an empty one.
///
/// Two spellings, told apart by the `:` that only one of them can hold: a bare device path is
/// the read-only disk, and `tag:/abin` is the share. Which one a boot sends follows from what
/// [`BootArgs::abin`] named — a file or a directory — and a guest path may not contain a `:`,
/// so the two can never be read for each other.
pub const ABIN_ENV: &str = "CORTEX_UVM_ABIN";

/// Where it is mounted, and what goes first on `PATH`.
pub const ABIN_PATH: &str = "/abin";

/// What to mount `/abin` from, and as what, given whatever [`ABIN_ENV`] says.
///
/// Here rather than in the guest that calls it, because the boot writes this variable and the
/// guest reads it: a rule for telling two spellings apart belongs beside the constant they are
/// spellings of, where one change moves both ends.
pub fn abin_mount(spec: &str) -> (&str, &str) {
    match spec.split_once(':') {
        Some((tag, _)) => (tag, "virtiofs"),
        None => (spec, "erofs"),
    }
}

/// The virtio-fs tag the context is attached under. Never seen by a caller: it is an
/// identifier two device configurations agree on, and the guest mounts it by this name.
pub const CONTEXT_TAG: &str = "cortexctx";

/// The tag a shared `/abin` is attached under, when [`BootArgs::abin`] named a directory.
pub const ABIN_TAG: &str = "cortexabin";

/// The tag the artifacts tree is attached under.
///
/// One device per tree rather than one device with both directories under it, because the
/// host has two directories and no common parent to serve: each is somewhere the caller
/// mounted it, and inventing a parent would mean the host arranging its own filesystem around
/// what this protocol happens to carry.
pub const ARTIFACTS_TAG: &str = "cortexart";

/// Where the guest finds the boot root once it is standing on the overlay instead.
///
/// The root the kernel handed over is kept rather than detached, because everything a commit
/// needs is under it: the upperdir at [`UPPER_DIR`], which the overlay offers under no other
/// name, and the share itself, which is how the layer leaves the guest at all.
pub const OLD_ROOT: &str = "/oldroot";

/// What a commit leaves in the boot root, and the name the host reads it back under.
///
/// A bare name and not a path, because the two ends reach that root by different names: the
/// host by the directory it made, the guest by [`OLD_ROOT`].
///
/// A file in the already-shared root and not a device of its own, for the reason
/// [`SNAPSHOT_PATH`] is one — this is that file in the other direction. A layer is neither
/// short nor bounded, so it cannot go back over the console channel, and a second virtio-fs
/// share would be a device, a tag and an argument for a directory the guest can already
/// write into at [`OLD_ROOT`].
pub const LAYER_TAR: &str = ".cortex-layer.tar";

/// Where a guest that kept its old root can reach the upperdir of the overlay it stands on.
///
/// The host cannot read that ext4 itself and the overlay offers the upper under no other
/// name, so this is the only way a commit sees what a session wrote. Under [`OLD_ROOT`],
/// spelled in full because a `const` cannot be assembled out of one.
pub const UPPER_DIR: &str = "/oldroot/mnt/upper/upper";

/// Where a boot leaves the layer a session is to start on, and where the guest unpacks it
/// from. Absent for a session that starts on its base alone.
///
/// The same tar a commit produces, read the other way round. That tar *is* an upperdir — the
/// files a session wrote, with its deletions as the whiteout nodes overlayfs left — so putting
/// one back into a bare upperdir is what makes a session carry on where another stopped.
///
/// A file in the boot root rather than a disk or an argument, for the reason
/// [`IMAGE_SPEC_PATH`] is one: the root is already shared, and a layer is neither short nor
/// bounded. The guest reads it before the pivot, which is the last moment the upperdir is a
/// bare directory — afterwards it has an overlay on top, and [`UPPER_DIR`] is what reaches
/// it.
pub const SNAPSHOT_PATH: &str = "/.cortex-snapshot.tar";

/// Where a boot writes [`ImageSpec`] in the boot root, and the path the guest reads it from.
///
/// A file rather than another argument, because what reaches the guest is the kernel
/// command line: libkrun passes it as `KRUN_ENV=…`, which is size-limited and cannot carry a
/// space, and an image's `ENV` is neither short nor free of them. The boot root is already
/// shared over virtio-fs, so a file in it costs nothing — and the guest reads it before the
/// pivot detaches that root.
pub const IMAGE_SPEC_PATH: &str = "/.cortex-image";

/// What the base image says about running a process in it, as BSON at [`IMAGE_SPEC_PATH`].
///
/// BSON because it is the codec both ends already carry for the console wire.
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

/// `PATH` for everything an execution spawns when the base image did not say — see
/// [`ImageSpec::env`].
///
/// Set here rather than inherited, because there is nothing to inherit it from: libkrun
/// hands the guest's first process the environment the boot named and nothing else, so a
/// guest without this line is one where `sh` cannot find `ls`.
pub const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// What a guest answers a `commit` with: the layer is written, and this big.
///
/// Not a [`CommitResp`](https://docs.rs/cortex/latest/cortex/console/struct.CommitResp.html),
/// which names an image — something only the host can make, and only after reading what this
/// wrote. The console server replaces this with one before the client sees anything, which is
/// what it already does with `init`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestCommit {
    /// The tar's size, so the host can tell an empty layer from a missing one.
    pub size: u64,
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

    /// A device, the gateway's resolver, and whatever [`host_ports`](BootArgs::host_ports) granted. Nothing else.
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
    /// Reaching the host is still only what [`host_ports`](BootArgs::host_ports) granted, which is the point of that
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

/// The interface the boot attached, as `iface=<name>,mac=<addr>,mtu=<n>`. Absent for a guest
/// with no network, which is the default.
///
/// **Not this project's spelling.** These are `microsandbox-network`'s own variable names, and
/// they arrive from the stack that assigned the values — passed through by the boot rather than
/// translated, so there is one description of the network and not two. Its guest agent reads
/// the same names to do the same job, which is what makes this the shape to match rather than
/// invent.
///
/// The `mac=` field is the one that is not description: it is how the guest tells the attached
/// interface from the others the kernel brought up on its own.
pub const NET_ENV: &str = "MSB_NET";

/// The addresses on that interface, as `addr=<ip>/<prefix>,gw=<ip>,dns=<ip>`.
///
/// A separate variable from [`NET_ENV`] because a stack with no IPv4 route to offer sends no
/// IPv4 — and one that has both sends `MSB_NET_IPV6` beside it, which this end does not read
/// yet.
pub const NET_IPV4_ENV: &str = "MSB_NET_IPV4";

/// Where a resolver is named on any system a base image was built for.
pub const RESOLV_CONF: &str = "/etc/resolv.conf";

/// Everything a console server tells a boot, as the boot's own command line.
///
/// # Why arguments and not the environment
///
/// The two halves of this file are two channels, and only one of them is constrained. What a
/// boot tells the *guest* has to be environment, because libkrun puts an exec's argv and env
/// alike on the kernel command line. What a *server* tells a boot is an ordinary
/// `Command::spawn`, and there was never a reason for it to be ambient.
///
/// **Ambient was the problem.** A child inherits its parent's environment, so a name the server
/// wrote for the boot was also a name an operator could set on the server, and one of them came
/// to mean two things. An argument cannot be inherited: everything here is either passed or
/// absent, and absent is a parse error rather than a silently different session.
///
/// The other half of it is that this is one type with one `to_args` and one `parse`, so a value
/// added to one side is a value the other side stops compiling without. The constants this
/// replaced were shared names read in two places, which is a weaker promise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootArgs {
    /// A directory served as the guest's virtio-fs root, holding the guest binary and the
    /// image spec and nothing else.
    pub boot_root: PathBuf,

    /// Where the child connects to reach the server. One socket, one connection, and its
    /// descriptor becomes the guest's console port.
    pub channel: PathBuf,

    /// The read-only base image, as a host path, and how the VMM has to read it. The two travel
    /// together because they are one fact: what the server provisioned.
    pub base: PathBuf,
    pub base_format: BaseFormat,

    /// The session's writable image, as a host path.
    pub session: PathBuf,

    /// The native executables to put at `/abin`, and `None` for a session that gets none.
    ///
    /// **A file or a directory, and the boot tells them apart by asking the filesystem.** A
    /// file is a raw EROFS attached as a read-only disk — never a descriptor, so there is no
    /// format to say alongside it: `/abin` is cortex's own executables and no others, which is
    /// one layer attached as it stands rather than anything stitched. A directory is shared
    /// over virtio-fs instead, which is what a host whose executables were just built has
    /// rather than an image of them.
    ///
    /// The two are one field because which one this is is a fact about the path rather than a
    /// choice to be stated twice. They are not the same offer, though: a virtio-fs share has
    /// no host-side read-only option and the guest is root inside itself, so a session can
    /// write back into a shared directory that every later session on this host will mount.
    /// The disk cannot be written to at all, which is why it is what a session gets when
    /// nobody is standing there rebuilding the executables.
    pub abin: Option<PathBuf>,

    /// The host directory to put in front of the guest, and `None` for a session that declared
    /// no tree. Mounted in the guest at [`CONTEXT_PATH`], which is where it is named from
    /// there on — see [`CONTEXT_ENV`].
    pub context: Option<PathBuf>,

    /// Where the session leaves what it produces, and `None` for a session that named none.
    /// Shared exactly as the context is, and mounted at [`ARTIFACTS_PATH`].
    pub artifacts: Option<PathBuf>,

    /// How much of a network the session gets. Decided by the server, because it decides a
    /// *device*, which is attached before a kernel comes up.
    /// Where the guest's own console goes — kernel messages, a panic, whatever a boot that
    /// failed has to say. A file rather than this process's stderr, so that a machine which
    /// never connected can be asked what happened.
    pub console: Option<PathBuf>,

    pub network: Network,

    /// TCP ports on the host this session may open, beyond whatever [`network`](Self::network)
    /// allows.
    ///
    /// # Why a grant of its own and not a wider [`Network`]
    ///
    /// Because the convenient way to say "the host" is `DestinationGroup::Host` with no ports on
    /// it, and a rule with no ports matches **every** port. Spelling host reach that way would
    /// mean a session that asked to talk to one service on this machine could talk to all of
    /// them — and, worse, that widening a session's *outside* reach silently widened its inside
    /// one.
    ///
    /// So the two are separate axes. `network` says how far out a session goes; this says which
    /// doors on this machine are open to it, one port at a time, TCP only. A session gets both,
    /// and neither implies the other.
    ///
    /// Meaningless without a device, so [`Network::Disabled`] with ports named is a
    /// contradiction the server refuses rather than a grant it quietly drops.
    pub host_ports: Vec<u16>,

    /// Guest vCPUs and memory, when the server was told to override them.
    pub vcpus: Option<u8>,
    pub memory_mib: Option<u32>,
}

impl BootArgs {
    /// The whole of a boot's argv, less the binary itself — the boot has no mode to select,
    /// so there is nothing in front of these.
    ///
    /// `OsString` throughout: a path that is not UTF-8 is still a path, and nothing here has to
    /// read one as text. The exceptions are the trees, which the guest is told about as
    /// strings, and that is refused where it is used rather than here.
    pub fn to_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();

        let mut put = |flag: &str, value: &OsStr| {
            args.push(OsString::from(flag));
            args.push(value.to_os_string());
        };

        put("--boot-root", self.boot_root.as_os_str());
        put("--channel", self.channel.as_os_str());
        put("--base", self.base.as_os_str());
        put("--base-format", OsStr::new(self.base_format.as_str()));
        put("--session", self.session.as_os_str());
        put("--network", OsStr::new(self.network.as_str()));
        if let Some(console) = &self.console {
            put("--console", console.as_os_str());
        }
        for port in &self.host_ports {
            put("--host-port", OsStr::new(&port.to_string()));
        }
        if let Some(context) = &self.context {
            put("--context", context.as_os_str());
        }
        if let Some(artifacts) = &self.artifacts {
            put("--artifacts", artifacts.as_os_str());
        }
        if let Some(abin) = &self.abin {
            put("--abin", abin.as_os_str());
        }
        if let Some(vcpus) = self.vcpus {
            put("--vcpus", OsStr::new(&vcpus.to_string()));
        }
        if let Some(memory) = self.memory_mib {
            put("--memory-mib", OsStr::new(&memory.to_string()));
        }
        args
    }

    /// Read them back, refusing anything this does not recognise.
    ///
    /// A boot is started by a console server and by nothing else, so a missing argument is a
    /// mismatch between two halves of one binary rather than a caller to be lenient with. It is
    /// refused before a hypervisor is touched, which is the cheapest place it can be.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<BootArgs> {
        let mut boot_root = None;
        let mut channel = None;
        let mut base = None;
        let mut base_format = None;
        let mut session = None;
        let mut network = None;
        let mut host_ports = Vec::new();
        let mut console = None;
        let mut context = None;
        let mut artifacts = None;
        let mut abin = None;
        let mut vcpus = None;
        let mut memory_mib = None;

        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let flag = flag
                .into_string()
                .map_err(|flag| anyhow::anyhow!("{flag:?} is not an argument this reads"))?;
            let mut value = || {
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("{flag} was given nothing to be"))
            };
            match flag.as_str() {
                "--console" => console = Some(PathBuf::from(value()?)),
                "--boot-root" => boot_root = Some(PathBuf::from(value()?)),
                "--channel" => channel = Some(PathBuf::from(value()?)),
                "--base" => base = Some(PathBuf::from(value()?)),
                "--base-format" => base_format = Some(text(&flag, value()?)?),
                "--session" => session = Some(PathBuf::from(value()?)),
                "--network" => network = Some(text(&flag, value()?)?),
                "--host-port" => host_ports.push(number(&flag, value()?)?),
                "--context" => context = Some(PathBuf::from(value()?)),
                "--artifacts" => artifacts = Some(PathBuf::from(value()?)),
                "--abin" => abin = Some(PathBuf::from(value()?)),
                "--vcpus" => vcpus = Some(number(&flag, value()?)?),
                "--memory-mib" => memory_mib = Some(number(&flag, value()?)?),
                other => anyhow::bail!("{other} is not an argument a boot takes"),
            }
        }

        let required = |name: &str, value: Option<PathBuf>| {
            value
                .ok_or_else(|| anyhow::anyhow!("{name} is missing — a boot is started by a server"))
        };
        Ok(BootArgs {
            boot_root: required("--boot-root", boot_root)?,
            channel: required("--channel", channel)?,
            base: required("--base", base)?,
            base_format: match base_format {
                Some(spelling) => BaseFormat::parse(&spelling)?,
                None => anyhow::bail!("--base-format is missing — a boot is started by a server"),
            },
            session: required("--session", session)?,
            network: match network {
                Some(name) => Network::parse(Some(&name))?,
                None => anyhow::bail!("--network is missing — a boot is started by a server"),
            },
            console,
            host_ports,
            context,
            artifacts,
            abin,
            vcpus,
            memory_mib,
        })
    }
}

/// An argument's value as text, for the few that are not paths.
fn text(flag: &str, value: OsString) -> anyhow::Result<String> {
    value
        .into_string()
        .map_err(|value| anyhow::anyhow!("{flag} is not utf-8: {value:?}"))
}

/// An argument's value as a number. Refused rather than defaulted: a server computed it, so a
/// value that is not one is a bug on this side of the process boundary.
fn number<T: std::str::FromStr>(flag: &str, value: OsString) -> anyhow::Result<T> {
    text(flag, value)?
        .parse()
        .map_err(|_| anyhow::anyhow!("{flag} is not a number"))
}

/// How the read-only base image is laid out on the host — the value of `--base-format`, and
/// what a boot turns into a disk format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BaseFormat {
    /// A single raw image: what a rootfs tarball is encoded to.
    #[default]
    Raw,
    /// A VMDK descriptor stitching per-layer blobs into one disk: what a registry pull's
    /// layered materialization produces.
    Vmdk,
}

impl BaseFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            BaseFormat::Raw => "raw",
            BaseFormat::Vmdk => "vmdk",
        }
    }

    /// Parse the spelling a server wrote. An unrecognised one is an error rather than the
    /// default, which would fail at the guest's mount instead — several seconds and a kernel
    /// log away from the mistake.
    pub fn parse(spelling: &str) -> anyhow::Result<BaseFormat> {
        match spelling {
            "raw" => Ok(BaseFormat::Raw),
            "vmdk" => Ok(BaseFormat::Vmdk),
            other => anyhow::bail!("--base-format: {other} is not `raw` or `vmdk`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> BootArgs {
        BootArgs {
            boot_root: "/tmp/boot-root".into(),
            channel: "/tmp/sock".into(),
            base: "/cache/oci/vmdk/sha256_7e6269.vmdk".into(),
            base_format: BaseFormat::Vmdk,
            session: "/tmp/session.ext4".into(),
            console: Some("/tmp/console.log".into()),
            network: Network::Public,
            host_ports: vec![8080, 3000],
            context: Some("/Users/someone/project".into()),
            artifacts: Some("/Users/someone/out".into()),
            abin: Some("/cache/layers/sha256_2b91c4.erofs".into()),
            vcpus: Some(4),
            memory_mib: Some(8192),
        }
    }

    /// A session with no `/abin` says nothing about one, and reads back as having none.
    #[test]
    fn a_session_with_no_abin_says_nothing_about_one() {
        let bare = BootArgs {
            abin: None,
            ..args()
        };
        let written = bare.to_args();
        assert!(!written.iter().any(|arg| arg == "--abin"), "{written:?}");
        let back = BootArgs::parse(written).unwrap();
        assert_eq!(back.abin, None);
    }

    /// A directory travels as the same argument an image does: which one a path is, is the
    /// filesystem's answer and not something either end spells differently.
    #[test]
    fn a_directory_of_executables_is_the_same_argument_as_an_image() {
        let built = BootArgs {
            abin: Some("/Users/someone/.cache/cortex/abin".into()),
            ..args()
        };
        let back = BootArgs::parse(built.to_args()).unwrap();
        assert_eq!(back, built);
    }

    /// The two spellings of [`ABIN_ENV`], told apart by the `:` a guest path cannot hold.
    ///
    /// Reading one for the other is a mount that fails at boot with a filesystem type the
    /// source is not — so the rule is tested where it is written rather than trusted at the
    /// two call sites that are in different binaries.
    #[test]
    fn a_tag_is_a_share_and_a_device_is_a_disk() {
        assert_eq!(
            abin_mount(&format!("{ABIN_TAG}:{ABIN_PATH}")),
            (ABIN_TAG, "virtiofs")
        );
        assert_eq!(abin_mount(GUEST_ABIN_DEV), (GUEST_ABIN_DEV, "erofs"));
    }

    /// Everything this writes is something it reads. Both ends are this type, so the compiler
    /// catches a field added to one side — what it cannot catch is a field written under one
    /// name and read under another, which is what this is for.
    #[test]
    fn boot_arguments_round_trip() {
        let sent = args();
        assert_eq!(
            BootArgs::parse(sent.to_args()).expect("its own spelling"),
            sent
        );
    }

    /// The optional ones are absent rather than empty, and come back absent.
    ///
    /// A session with no tree is the case that matters: an empty `--context` would be a
    /// directory named by the empty string, and the boot would try to share it. The same goes
    /// for the tree beside it, which is shared by exactly the same mechanism.
    #[test]
    fn what_was_not_said_is_not_sent() {
        let bare = BootArgs {
            context: None,
            artifacts: None,
            vcpus: None,
            memory_mib: None,
            ..args()
        };
        let spelled = bare.to_args();
        for flag in ["--context", "--artifacts", "--vcpus", "--memory-mib"] {
            assert!(
                !spelled.iter().any(|arg| arg == flag),
                "{flag} was sent anyway"
            );
        }
        assert_eq!(BootArgs::parse(spelled).expect("its own spelling"), bare);
    }

    /// A boot is started by a console server and by nothing else, so anything missing or
    /// unrecognised is two halves of one binary disagreeing. Refused before a hypervisor is
    /// touched, which is the cheapest place it can be.
    #[test]
    fn a_boot_refuses_arguments_a_server_would_not_have_sent() {
        let mut short = args().to_args();
        short.truncate(2); // `--boot-root` and its value, and nothing else a boot needs
        assert!(
            BootArgs::parse(short).is_err(),
            "a missing argument was accepted"
        );

        let mut unknown = args().to_args();
        unknown.push("--turbo".into());
        assert!(
            BootArgs::parse(unknown).is_err(),
            "an unknown argument was accepted"
        );

        let mut dangling = args().to_args();
        dangling.push("--vcpus".into());
        assert!(
            BootArgs::parse(dangling).is_err(),
            "a flag with no value was accepted"
        );

        let mut nonsense = args();
        nonsense.vcpus = None;
        let mut spelled = nonsense.to_args();
        spelled.extend(["--vcpus".into(), "several".into()]);
        assert!(
            BootArgs::parse(spelled).is_err(),
            "a vcpu count that is not one was accepted"
        );
    }

    /// Every spelling this writes is one it reads. The two halves are a boot apart — a server
    /// writes the name and a child in another process parses it — so a variant added with one
    /// of them updated fails at a mount, in a kernel log, seconds later.
    #[test]
    fn a_base_format_round_trips_through_its_name() {
        for format in [BaseFormat::Raw, BaseFormat::Vmdk] {
            assert_eq!(
                BaseFormat::parse(format.as_str()).expect("its own name"),
                format
            );
        }
    }

    #[test]
    fn an_unknown_base_format_is_refused() {
        assert!(BaseFormat::parse("qcow2").is_err());
        assert!(BaseFormat::parse("").is_err());
    }

    /// The same promise for a reach: a server writes the name and a binary in another process
    /// parses it, so a variant added with only one of them updated is a session that quietly
    /// gets a reach nobody asked for.
    #[test]
    fn a_reach_round_trips_through_its_name() {
        for reach in [
            Network::Disabled,
            Network::HostOnly,
            Network::Public,
            Network::Full,
        ] {
            assert_eq!(
                Network::parse(Some(reach.as_str())).expect("its own name"),
                reach
            );
        }
    }

    /// Silence is the default, and a misspelling is neither answer. `on`, `true` and `1` are all
    /// things a caller might type and none of them says how much reach is wanted.
    #[test]
    fn a_reach_that_is_not_one_is_refused() {
        assert_eq!(Network::parse(None).expect("unset"), Network::default());
        assert_eq!(Network::parse(Some("")).expect("empty"), Network::default());
        for typo in ["hsot", "on", "true", "1", "HOST"] {
            assert!(Network::parse(Some(typo)).is_err(), "{typo} was accepted");
        }
    }

    /// A grant round trips, and silence is no grant. Same reasoning as the reach above: the two
    /// halves are a boot apart, so a list that spelled one way and read another is a session
    /// whose doors are not the ones anybody named.
    #[test]
    fn a_host_port_grant_round_trips_and_defaults_to_nothing() {
        for ports in [vec![], vec![8080], vec![8080, 3000, 65535]] {
            let granted = BootArgs {
                host_ports: ports.clone(),
                ..args()
            };
            assert_eq!(
                BootArgs::parse(granted.to_args())
                    .expect("its own spelling")
                    .host_ports,
                ports,
            );
        }

        // No grant is no flag, which is what an empty list has to spell: `--host-port` with
        // nothing after it would be a port named by the empty string.
        let none = BootArgs {
            host_ports: Vec::new(),
            ..args()
        };
        assert!(!none.to_args().iter().any(|arg| arg == "--host-port"));

        // A port is a number and nothing else — 65536 is not one, and neither is a name.
        for typo in ["http", "65536", "-1"] {
            let mut spelled = none.to_args();
            spelled.extend(["--host-port".into(), typo.into()]);
            assert!(BootArgs::parse(spelled).is_err(), "{typo} was accepted");
        }
    }
}
