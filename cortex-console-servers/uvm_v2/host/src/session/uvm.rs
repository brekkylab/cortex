//! A micro-VM, booted on an image built here.
//!
//! Two things start one, and they differ in how long it lives rather than in what it does.
//! A session boots once and runs whatever its client asks for until the client is done; a
//! build's `RUN` wants one command and the filesystem it left behind. So the long-lived one
//! is a value, and the short one is a function over it.
//!
//! Either way the guest is `cortex-uvm-v2-guest`, which libkrun execs as the first userspace
//! process — in `boot` mode for a session, `exec` mode for a build step.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::contract::Network;
use crate::rootfs::{Digest, Image, home};

/// What a command exited with, and what it wrote.
pub struct Exit {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A host directory the guest can see, and where it sees it.
pub struct Mount {
    pub at: String,
    pub from: PathBuf,
}

/// A booted micro-VM, held for as long as there is something to ask it.
///
/// Dropping it takes the machine down and removes the scratch it was given, which is what
/// makes a session's writes go no further than the session.
pub struct Uvm {
    _private: (),
}

impl Uvm {
    /// Boot `image` and wait for the guest to say it is there.
    ///
    /// Returns once the guest has built its root and opened the port, not merely once the
    /// machine is running — there is nothing a caller can do with the time in between.
    pub async fn boot(image: &Image, mounts: &[Mount]) -> anyhow::Result<Uvm> {
        // The executable a boot runs is never this process: entitlements are read at
        // `exec`, so they are on a file before it starts or they are nowhere. And a copy
        // rather than this file itself, because signing a binary while it runs would leave
        // the running process unsigned against a file that no longer matches it.
        //
        // Named by the bytes it holds, so a rebuild signs again and two boots of one build
        // share the copy.
        use std::os::unix::fs::PermissionsExt as _;

        let exe = std::env::current_exe()?;
        let bytes =
            std::fs::read(&exe).map_err(|e| anyhow::anyhow!("reading {}: {e}", exe.display()))?;
        let name = Digest::of(&bytes).file_stem()[..16].to_string();

        let cache = home().join("boot");
        std::fs::create_dir_all(&cache)?;
        let machine = cache.join(&name);
        if !machine.is_file() {
            // Written, made executable and signed at a path nothing else will pick, then
            // renamed into place — two boots racing here both do the work and the rename
            // decides which copy survives, where sharing one path would mean signing a file
            // the other was still writing.
            let part = cache.join(format!("{name}.{}.part", std::process::id()));
            std::fs::write(&part, &bytes)?;
            std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))?;

            #[cfg(target_os = "macos")]
            if let Err(e) = super::entitlement::sign(&part) {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }

            std::fs::rename(&part, &machine)?;
        }

        todo!(
            "{} as a disk and an upper, a boot root, and {machine:?} started on them with {} mount(s)",
            image.layers.len(),
            mounts.len()
        )
    }

    /// Run one command, with what the image states already in front of it.
    pub async fn exec(&mut self, argv: &[String], timeout_ms: Option<u64>) -> anyhow::Result<Exit> {
        todo!("{argv:?} with {timeout_ms:?}")
    }

    /// Everything written since the boot, kept as a layer.
    pub async fn commit(&mut self) -> anyhow::Result<Digest> {
        todo!("the guest's upperdir, as an EROFS in the store")
    }
}

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

    /// Whether this session may commit, which decides one thing at boot and nothing after:
    /// the old root is kept rather than detached, so the guest can still reach the upperdir of
    /// the overlay it is standing on.
    pub committable: bool,

    /// A directory on this host the guest may write a commit's layer into, shared in at
    /// [`COMMIT_PATH`](crate::contract::COMMIT_PATH). `None` for a session that cannot commit.
    ///
    /// Writable, unlike the `/abin` disk, and that is not a contradiction: `/abin` is a cache
    /// every later session reads, where this is one session's scratch — made by the server,
    /// thrown away with the session, and holding nothing but that session's own output.
    pub commit_out: Option<PathBuf>,

    /// A read-only image of native executables to mount at `/abin`.
    ///
    /// `None` is a session that gets none. Always a raw EROFS and never a descriptor, so
    /// there is no format to say alongside it: `/abin` is cortex's own executables and no
    /// others, which is one layer attached as it stands rather than anything stitched.
    pub abin: Option<PathBuf>,

    /// The host directory to put in front of the guest, and `None` for a session that declared
    /// no tree. Mounted in the guest at [`CONTEXT_PATH`](crate::contract::CONTEXT_PATH), which is where it is named from
    /// there on — see [`CONTEXT_ENV`](crate::contract::CONTEXT_ENV).
    pub context: Option<PathBuf>,

    /// Where the session leaves what it produces, and `None` for a session that named none.
    /// Shared exactly as the context is, and mounted at [`ARTIFACTS_PATH`](crate::contract::ARTIFACTS_PATH).
    pub artifacts: Option<PathBuf>,

    /// Room for the session to work in, and `None` for a session that named none. Shared as
    /// the two above are, mounted at [`SCRATCH_PATH`](crate::contract::SCRATCH_PATH), and additionally **where the guest
    /// stands** — see [`SCRATCH_ENV`](crate::contract::SCRATCH_ENV).
    pub scratch: Option<PathBuf>,

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
    /// The arguments a server spawns a boot with, after the mode argument.
    ///
    /// `OsString` throughout: a path that is not UTF-8 is still a path, and nothing here has to
    /// read one as text. The exceptions are the three trees, which the guest is told about as
    /// strings, and that is refused where it is used rather than here.
    pub fn to_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();

        // First, before the closure below takes its borrow of `args`. This is the one flag
        // that carries no value — it is a fact about the session rather than a thing with a
        // spelling — so it cannot go through `put`, and where it sits does not matter to
        // `parse`.
        if self.committable {
            args.push(OsString::from("--committable"));
        }

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
        if let Some(context) = &self.context {
            put("--context", context.as_os_str());
        }
        if let Some(artifacts) = &self.artifacts {
            put("--artifacts", artifacts.as_os_str());
        }
        if let Some(scratch) = &self.scratch {
            put("--scratch", scratch.as_os_str());
        }
        if let Some(abin) = &self.abin {
            put("--abin", abin.as_os_str());
        }
        if let Some(out) = &self.commit_out {
            put("--commit-out", out.as_os_str());
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
        let mut context = None;
        let mut artifacts = None;
        let mut scratch = None;
        let mut abin = None;
        let mut committable = false;
        let mut commit_out = None;
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
                "--context" => context = Some(PathBuf::from(value()?)),
                "--artifacts" => artifacts = Some(PathBuf::from(value()?)),
                "--scratch" => scratch = Some(PathBuf::from(value()?)),
                "--abin" => abin = Some(PathBuf::from(value()?)),
                "--committable" => committable = true,
                "--commit-out" => commit_out = Some(PathBuf::from(value()?)),
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
            context,
            artifacts,
            scratch,
            committable,
            commit_out,
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

/// How the read-only base image is laid out on the host — the value of `BASE_FORMAT_ENV`,

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
            context: Some("/Users/someone/project".into()),
            artifacts: Some("/Users/someone/out".into()),
            scratch: Some("/Users/someone/scratch".into()),
            abin: Some("/cache/layers/sha256_2b91c4.erofs".into()),
            committable: true,
            commit_out: Some("/tmp/cortex-uvm-commit".into()),
            vcpus: Some(4),
            memory_mib: Some(8192),
        }
    }

    /// A committable session says both things, and an ordinary one says neither.
    ///
    /// The pair matters. A kept root with nowhere to write is a guest that can see what the
    /// session wrote and cannot hand it over; somewhere to write with no kept root is the
    /// reverse.
    #[test]
    fn a_committable_boot_says_so_and_names_somewhere_to_write() {
        let args = args();
        let back = BootArgs::parse(args.to_args()).unwrap();
        assert!(back.committable);
        assert_eq!(back.commit_out, args.commit_out);
    }

    #[test]
    fn an_ordinary_boot_says_neither() {
        let plain = BootArgs {
            committable: false,
            commit_out: None,
            ..args()
        };
        let written = plain.to_args();
        assert!(
            !written
                .iter()
                .any(|arg| arg == "--committable" || arg == "--commit-out"),
            "{written:?}"
        );
        let back = BootArgs::parse(written).unwrap();
        assert!(!back.committable);
        assert_eq!(back.commit_out, None);
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
    /// for the two trees beside it, which are shared by exactly the same mechanism.
    #[test]
    fn what_was_not_said_is_not_sent() {
        let bare = BootArgs {
            context: None,
            artifacts: None,
            scratch: None,
            vcpus: None,
            memory_mib: None,
            ..args()
        };
        let spelled = bare.to_args();
        for flag in [
            "--context",
            "--artifacts",
            "--scratch",
            "--vcpus",
            "--memory-mib",
        ] {
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
