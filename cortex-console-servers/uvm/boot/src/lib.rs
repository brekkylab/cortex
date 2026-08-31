//! What a console server tells a boot, and what a boot tells the guest.
//!
//! Two cross-process contracts live together here, in a crate of their own because the two
//! ends of the first one are two binaries. They are not the same kind of thing:
//!
//! - **the server to the boot process** ([`BootArgs`]). An ordinary `Command::spawn`, so it
//!   is the boot's command line. Both ends take this crate, and both are this one type, so
//!   nothing there can drift. It is also the whole of what they say to each other: a boot is
//!   started, told, and never asked anything, which is what made `cortex-uvm-boot` a separate
//!   binary rather than a rewrite.
//! - **the boot process to the guest** ([`LOWER_ENV`], [`UPPER_ENV`], [`SHARE_ENV`],
//!   [`GUEST_BIN_PATH`], [`PORT_NAME`], [`HANDSHAKE`]). The other end of
//!   that one is `cortex-uvm-guest`'s `contract` module, which **has to change with this
//!   file** — that crate is built for a different target and takes nothing it does not need,
//!   so the compiler will not notice a mismatch.
//!
//! Where the tree lands inside the guest is **the host's own path for it**, carried in
//! [`SHARE_ENV`] like any other share. Not a constant of this crate's choosing: one spelling
//! on both sides of the hypervisor is what lets a `cwd` the guest reports be a path the
//! client can open, with nothing in the middle translating and nothing to disagree about.
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
/// execs. Also where the guest puts a copy of itself after its pivot, which is what the
/// delegated names end up symlinked to.
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

/// The overlay's upper, as the guest sees it.
pub const GUEST_UPPER_DEV: &str = "/dev/vda";

/// Told to the guest as `CORTEX_UVM_LOWER`, which is the name its `contract` module reads.
pub const LOWER_ENV: &str = "CORTEX_UVM_LOWER";

/// Told to the guest as `CORTEX_UVM_UPPER`.
pub const UPPER_ENV: &str = "CORTEX_UVM_UPPER";

/// Told to the guest as `CORTEX_UVM_SHARE`, spelled `tag:/guest/path`.
pub const SHARE_ENV: &str = "CORTEX_UVM_SHARE";

/// The virtio-fs tag the tree is attached under. Never seen by a caller: it is an
/// identifier two device configurations agree on, and the guest mounts it by this name.
pub const WORKFS_TAG: &str = "cortexws";

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
    /// The libkrunfw kernel the child boots.
    pub kernel: PathBuf,

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

    /// The host directory to put in front of the guest, and `None` for a session that declared
    /// no tree. Mounted in the guest at **this same path** — see [`SHARE_ENV`].
    pub workfs: Option<PathBuf>,

    /// How much of a network the session gets. Decided by the server, because it decides a
    /// *device*, which is attached before a kernel comes up.
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
    /// The arguments a server spawns a boot with, after [`BOOT_ARG`](crate::server::BOOT_ARG).
    ///
    /// `OsString` throughout: a path that is not UTF-8 is still a path, and nothing here has to
    /// read one as text. The one exception is `workfs`, which the guest is told about as a
    /// string, and that is refused where it is used rather than here.
    pub fn to_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        let mut put = |flag: &str, value: &OsStr| {
            args.push(OsString::from(flag));
            args.push(value.to_os_string());
        };

        put("--kernel", self.kernel.as_os_str());
        put("--boot-root", self.boot_root.as_os_str());
        put("--channel", self.channel.as_os_str());
        put("--base", self.base.as_os_str());
        put("--base-format", OsStr::new(self.base_format.as_str()));
        put("--session", self.session.as_os_str());
        put("--network", OsStr::new(self.network.as_str()));
        for port in &self.host_ports {
            put("--host-port", OsStr::new(&port.to_string()));
        }
        if let Some(workfs) = &self.workfs {
            put("--workfs", workfs.as_os_str());
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
        let mut kernel = None;
        let mut boot_root = None;
        let mut channel = None;
        let mut base = None;
        let mut base_format = None;
        let mut session = None;
        let mut network = None;
        let mut host_ports = Vec::new();
        let mut workfs = None;
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
                "--kernel" => kernel = Some(PathBuf::from(value()?)),
                "--boot-root" => boot_root = Some(PathBuf::from(value()?)),
                "--channel" => channel = Some(PathBuf::from(value()?)),
                "--base" => base = Some(PathBuf::from(value()?)),
                "--base-format" => base_format = Some(text(&flag, value()?)?),
                "--session" => session = Some(PathBuf::from(value()?)),
                "--network" => network = Some(text(&flag, value()?)?),
                "--host-port" => host_ports.push(number(&flag, value()?)?),
                "--workfs" => workfs = Some(PathBuf::from(value()?)),
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
            kernel: required("--kernel", kernel)?,
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
            host_ports,
            workfs,
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

/// How the read-only base image is laid out on the host — the value of [`BASE_FORMAT_ENV`],
/// and what a boot turns into a disk format.
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

    /// Parse the environment's spelling. An unrecognised one is an error rather than the
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

/// Where a boot writes [`ImageSpec`] in the boot root, and the path the guest reads it from.
///
/// A file rather than another environment value, because the environment here *is* the
/// kernel command line: libkrun passes it as `KRUN_ENV=…`, which is size-limited and cannot
/// carry a space, and an image's `ENV` is neither short nor free of them. The boot root is
/// already shared over virtio-fs, so a file in it costs nothing — and the guest reads it
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

/// How much of the network a session may reach: the value of [`BootArgs::network`].
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> BootArgs {
        BootArgs {
            kernel: "/opt/lib/libkrunfw.dylib".into(),
            boot_root: "/tmp/boot-root".into(),
            channel: "/tmp/sock".into(),
            base: "/cache/oci/vmdk/sha256_7e6269.vmdk".into(),
            base_format: BaseFormat::Vmdk,
            session: "/tmp/session.ext4".into(),
            network: Network::Public,
            host_ports: vec![8080, 3000],
            workfs: Some("/Users/someone/project".into()),
            vcpus: Some(4),
            memory_mib: Some(8192),
        }
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
    /// A session with no tree is the case that matters: an empty `--workfs` would be a
    /// directory named by the empty string, and the boot would try to share it.
    #[test]
    fn what_was_not_said_is_not_sent() {
        let bare = BootArgs {
            workfs: None,
            vcpus: None,
            memory_mib: None,
            ..args()
        };
        let spelled = bare.to_args();
        for flag in ["--workfs", "--vcpus", "--memory-mib"] {
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
        short.truncate(2); // just `--kernel <path>`
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
