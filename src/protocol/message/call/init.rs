use std::{path::Path, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::image::ImageSource;

/// What a session is. The `params` of `init`.
///
/// Everything here outlives one execution and is part of the environment rather than the
/// command (a tree must exist before a path can name a file in it; the base, network and
/// machine are fixed before a VM kernel starts), so it is said once, not per
/// [`ExecCall`](super::ExecCall).
///
/// # Trees
///
/// [`mounts`](Self::mounts) lists every tree the session gets, each a [`MountSpec`]: where
/// to get it, the absolute path it appears at, and whether commands may write in it.
///
/// **A tree's purpose is the client's and is not on the wire.** A project to read and an
/// output directory differ only in URL, path and `ro`; per-purpose members would cap
/// sessions at the purposes enumerated here. Trees are separate namespaces the client
/// places itself, not stores composed under one root.
///
/// No tree is needed for scratch space: a session already stands on a writable filesystem
/// that goes away with it. An empty list is a valid session that sees only the executor's
/// own filesystem.
///
/// # Machine
///
/// [`vcpus`](Self::vcpus), [`memory_mib`](Self::memory_mib), [`gpu`](Self::gpu),
/// [`gpu_memory_mib`](Self::gpu_memory_mib) and [`disk_gib`](Self::disk_gib) shape the
/// machine, which is made before the first command and outlives the last.
///
/// **A server provides exactly what is named or refuses** with
/// [`UNSUPPORTED_MACHINE`](crate::protocol::Error::UNSUPPORTED_MACHINE) at `init`: a
/// silently smaller machine would leave the client inferring its shape from how long
/// commands take.
///
/// Each is separately optional; `None` (the common case) is the server's own default,
/// since only it knows what its host has. `0` for any numeric member is
/// [`INVALID_PARAMS`](crate::protocol::Error::INVALID_PARAMS), not another way to say `None`.
///
/// There is no per-command share, affinity or limit: the machine is the unit handed out,
/// and two sizes means two sessions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitCall {
    /// The base a session's commands run in. Required by VM backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSource>,

    /// What an earlier session changed from the base image, as a
    /// [`snapshot`](super::SnapshotCall) returned it; the session starts with those changes in place.
    ///
    /// **A layer tar**: the files written, with deletions as OCI whiteouts. Not a filesystem
    /// image, whose metadata, journal and formatted free space would not fit a frame; and a
    /// layer is something an executor already knows how to put over a base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Vec<u8>>,

    /// Whether the session's commands reach a network at all. On unless said otherwise,
    /// since on is the one value every server can give.
    ///
    /// **On or off, nothing between.** On is what a process on the server's machine reaches,
    /// less that machine's own loopback, whose services are the operator's. A level between
    /// would be a firewall every backend reimplemented; where a session must be kept off part
    /// of a network, the server's machine already knows how.
    ///
    /// A server that cannot honour it refuses with
    /// [`UNSUPPORTED_NETWORK`](crate::protocol::Error::UNSUPPORTED_NETWORK): one whose commands
    /// run on this host cannot take the network away, so it refuses `false`.
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub network: bool,

    /// Ports on the server's machine that lead into the session, spelled as docker's `-p`
    /// (see [`Port`]). Empty publishes none.
    ///
    /// **Only in, and only TCP.** Reaching out is [`network`](Self::network)'s; this is the
    /// way in that a program serving something (a VNC server, a dev server, a notebook) needs.
    ///
    /// Ports with the network off are a contradiction, refused with
    /// [`INVALID_PARAMS`](crate::protocol::Error::INVALID_PARAMS) rather than dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<Port>,

    /// How many vCPUs the session's machine gets. `None` leaves it to the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u8>,

    /// Memory for the session's machine, in MiB. `None` leaves it to the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,

    /// Whether the session's commands get a GPU. `None` leaves it to the server.
    ///
    /// **A boolean, not a device:** what is attached is the backend's (virtio-gpu with
    /// Vulkan on one, the host's GPU on another), and naming a model or API would be a
    /// promise only some builds could keep. A command that needs a specific device inspects
    /// what is there.
    ///
    /// **`false` differs from `None`:** it forbids a GPU on a backend that would otherwise
    /// attach one, saving the device, renderer and boot time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<bool>,

    /// Memory the session's GPU may hold, in MiB. `None` leaves it to the server.
    ///
    /// **In addition to [`memory_mib`](Self::memory_mib), not a share of it**, even on a GPU
    /// that shares host memory (its mapped buffers are not counted as machine RAM).
    ///
    /// The attached device reports exactly this size to commands, so a program sizes itself
    /// up front instead of failing an allocation.
    ///
    /// [`INVALID_PARAMS`](crate::protocol::Error::INVALID_PARAMS) without a GPU: beside a
    /// [`gpu`](Self::gpu) of `false`, or `None` on a server that gives none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_memory_mib: Option<u32>,

    /// How much the session's commands may write, in GiB (MiB is too fine for a disk).
    /// `None` leaves it to the server.
    ///
    /// **Room for writes, not image size:** it bounds what the session adds over the base,
    /// including an `init` [`snapshot`](Self::snapshot); writing past it hits a full disk.
    ///
    /// **A ceiling, not an allocation:** the host pays for what was written, not what was
    /// asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gib: Option<u32>,

    /// **Ordered** to nest trees: mounts are realized in order, so `/work/out` after `/work`
    /// lands inside it, and not the other way round. Nothing else depends on order.
    ///
    /// Duplicate paths, or a path this server cannot use, are
    /// [`INVALID_PARAMS`](crate::protocol::Error::INVALID_PARAMS).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<MountSpec>,
}

impl Default for InitCall {
    fn default() -> Self {
        InitCall {
            image: None,
            snapshot: None,
            network: true,
            ports: Vec::new(),
            vcpus: None,
            memory_mib: None,
            gpu: None,
            gpu_memory_mib: None,
            disk_gib: None,
            mounts: Vec::new(),
        }
    }
}

fn on() -> bool {
    true
}

fn is_on(network: &bool) -> bool {
    *network
}

/// A tree a session is given: where to get it, where it appears, and whether it may be
/// written.
///
/// # The spelling
///
/// ```text
/// <host url>:<guest path>[:<option>]…
///
/// file:///srv/project:/work:ro
/// file:///srv/out:/work/out
/// ```
///
/// **One string**, in the source/destination/options order `mount`, `fstab` and container
/// runtimes use, so it reads familiarly and new options do not grow the wire schema.
///
/// # The scheme is the kind
///
/// | scheme | is |
/// |---|---|
/// | `file:///srv/project` | a directory on the server's own filesystem |
/// | `http://…`, `https://…` | a tree reached over HTTP — **on the wire, implemented nowhere** |
///
/// A URL, not a tagged object: the protocol only hands it to whatever realizes that kind,
/// so the schema does not grow with providers. A peer that has never heard of a scheme
/// still parses it and refuses it with
/// [`UNSUPPORTED_MOUNT`](crate::protocol::Error::UNSUPPORTED_MOUNT).
///
/// # The guest path is the client's to choose
///
/// [`guest_path`](Self::guest_path) is where the tree appears to commands, chosen by the
/// end that spells paths under it, so every [`read`](super::ReadCall), [`write`](super::WriteCall) and
/// command path is known before `init` goes out and nothing has to be read back.
///
/// It is absolute; a relative one would have no working directory to resolve against.
///
/// # The options
///
/// | option | means |
/// |---|---|
/// | `ro` | the session reads this tree and does not write in it |
/// | `rw` | a command may write in it — the default, and sayable so a client can be explicit |
///
/// **Read-only is per mount.** A [`write`](super::WriteCall) under an `ro` mount is refused with
/// [`IO_FAILED`](crate::protocol::Error::IO_FAILED), as a read-only filesystem would. A VM
/// backend mounts the tree read-only so commands' writes fail too; a host backend can only
/// enforce it on the calls it performs itself, and says so.
///
/// # How it is read
///
/// From the right: trailing option segments, then the first segment starting with `/` is
/// the guest path, and the rest is the host URL. This lets a URL carry its own colons (a
/// port) without quoting, at the cost that the guest path is absolute and colon-free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountSpec {
    host_url: String,
    guest_path: String,
    readonly: bool,
}

impl MountSpec {
    /// A writable mount of `host_url` at `guest_path`.
    ///
    /// Validated here so every value round-trips through its string spelling.
    pub fn new(
        host_url: impl Into<String>,
        guest_path: impl Into<String>,
    ) -> Result<Self, InvalidMount> {
        let host_url = host_url.into();
        let guest_path = guest_path.into();

        let refuse = |why| {
            Err(InvalidMount {
                why,
                spec: format!("{host_url}:{guest_path}"),
            })
        };

        match host_url.split_once("://") {
            None => return refuse("a mount URL needs a scheme"),
            Some((_, "")) => return refuse("a mount URL needs something after its scheme"),
            Some(_) => {}
        }
        if !guest_path.starts_with('/') {
            return refuse("a guest path is absolute");
        }
        if guest_path.contains(':') {
            return refuse("a guest path cannot carry a colon");
        }

        Ok(MountSpec {
            host_url,
            guest_path,
            readonly: false,
        })
    }

    /// The same mount, read-only (`ro`).
    pub fn read_only(mut self) -> Self {
        self.readonly = true;
        self
    }

    /// Where to get the tree, e.g. `file:///srv/project`.
    pub fn host_url(&self) -> &str {
        &self.host_url
    }

    /// Where it appears to the session's commands.
    pub fn guest_path(&self) -> &Path {
        Path::new(&self.guest_path)
    }

    /// Whether a write in this tree is refused.
    pub fn is_read_only(&self) -> bool {
        self.readonly
    }

    /// The scheme, which is the kind: `"file"`, `"https"`. A server picks a provider by it.
    pub fn scheme(&self) -> &str {
        self.host_url
            .split_once("://")
            .map_or(&self.host_url, |(s, _)| s)
    }

    /// The directory a `file://` URL names, or `None` for any other scheme.
    ///
    /// The path is what follows the scheme, **not percent-decoded**. The caller checks that
    /// it is absolute, since a relative path is a malformed request, not a missing provider.
    ///
    /// Shared here so every server reads a `file://` URL the same way.
    pub fn file_path(&self) -> Option<&Path> {
        self.host_url.strip_prefix("file://").map(Path::new)
    }
}

impl std::fmt::Display for MountSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}:{}", self.host_url, self.guest_path)?;
        if self.readonly {
            f.write_str(":ro")?;
        }
        Ok(())
    }
}

impl FromStr for MountSpec {
    type Err = InvalidMount;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let refuse = |why| {
            Err(InvalidMount {
                why,
                spec: spec.to_string(),
            })
        };

        // Search past the URL's own `://`, so a missing scheme is refused first rather than
        // read as a path.
        let Some(scheme) = spec.find("://") else {
            return refuse("a mount URL needs a scheme");
        };
        let (head, mut rest) = spec.split_at(scheme + "://".len());

        let mut readonly = false;
        loop {
            let Some((before, last)) = rest.rsplit_once(':') else {
                return refuse("a mount needs a guest path to appear at");
            };
            // An absolute guest path ends the options, so the URL may carry its own colons.
            if last.starts_with('/') {
                let mount = MountSpec::new(format!("{head}{before}"), last)?;
                return Ok(if readonly { mount.read_only() } else { mount });
            }
            match last {
                "ro" => readonly = true,
                "rw" => readonly = false,
                // Also covers a guest path missing its leading slash: neither is an option.
                _ => return refuse("a mount trails an absolute guest path with `ro` or `rw`"),
            }
            rest = before;
        }
    }
}

impl Serialize for MountSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MountSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spec = String::deserialize(deserializer)?;
        spec.parse().map_err(de::Error::custom)
    }
}

/// Why a string is not a [`MountSpec`].
///
/// Carries the offending string, since a session names several trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidMount {
    why: &'static str,
    spec: String,
}

impl std::fmt::Display for InvalidMount {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}: {:?}", self.why, self.spec)
    }
}

impl std::error::Error for InvalidMount {}

/// A port on the server's machine that leads into the session: `<host>:<console>`.
///
/// ```text
/// "8080:80"   127.0.0.1:8080 on the server's machine reaches port 80 in the session
/// ```
///
/// **Docker's spelling, host first**, since a second order for the same two numbers would
/// publish a session backwards. No address (the port is on loopback only) and no `/udp` (a
/// port is TCP).
///
/// # Loopback, held for the whole session
///
/// The server listens at `127.0.0.1:<host>` from `init` until the session ends, across every
/// `stop` and the boot after it. A connection arriving while nothing is booted waits for the
/// next boot. It reaches whatever in the session listens on `console`, on any address
/// including its own loopback, and is closed on accept when nothing does.
///
/// # Both numbers, always
///
/// The host port is the client's choice, so the server has nothing to answer about it. Port
/// `0` on either side is refused, and so is a host port already taken, at `init` while the
/// client can still pick another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Port {
    /// The port on the server's machine a connection is made to.
    pub host: u16,

    /// The port in the session a connection reaches.
    pub console: u16,
}

impl Port {
    /// `console` in the session, reached at `host` on the server's machine. Port 0 on either
    /// side is no port, and is refused here as it is on the wire.
    pub fn new(host: u16, console: u16) -> Result<Self, InvalidPort> {
        if host == 0 || console == 0 {
            return Err(InvalidPort {
                why: "port 0 is no port",
                spec: format!("{host}:{console}"),
            });
        }
        Ok(Port { host, console })
    }
}

impl std::fmt::Display for Port {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.console)
    }
}

impl FromStr for Port {
    type Err = InvalidPort;

    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let invalid = |why| InvalidPort {
            why,
            spec: spec.to_string(),
        };
        let number = |part: &str| {
            part.parse::<u16>()
                .map_err(|_| invalid("not a port number"))
        };
        let (host, console) = spec
            .split_once(':')
            .ok_or_else(|| invalid("not <host>:<console>"))?;
        Port::new(number(host)?, number(console)?).map_err(|_| invalid("port 0 is no port"))
    }
}

impl Serialize for Port {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Port {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spec = String::deserialize(deserializer)?;
        spec.parse().map_err(de::Error::custom)
    }
}

/// Why a string is not a [`Port`], with the string it was reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidPort {
    why: &'static str,
    spec: String,
}

impl std::fmt::Display for InvalidPort {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}: {:?}", self.why, self.spec)
    }
}

impl std::error::Error for InvalidPort {}

/// What the server made of the session. The `result` of `init`.
///
/// Mount locations are not echoed: each [`MountSpec`] already says where
/// its tree appears.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitResp {
    /// The session's starting working directory: the one the base image declared, or the
    /// server's default if it declared none. `None` if the server will not say.
    ///
    /// **The image's, by convention**, since it describes what the image was built to run;
    /// starting in a client tree would make it (maybe a read-only project) the default
    /// target of every relative path. A server that differs just reports it here.
    ///
    /// The server keeps the current directory, so an [`ExecCall`](super::ExecCall) names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A port is docker's string on the wire, host first, with both numbers said.
    #[test]
    fn a_port_is_spelled_the_way_docker_spells_it() {
        assert_eq!("8080:80".parse(), Port::new(8080, 80));
        assert_eq!(
            serde_json::to_value(Port::new(5901, 5900).unwrap()).unwrap(),
            serde_json::json!("5901:5900")
        );
        assert_eq!(
            serde_json::from_value::<Port>(serde_json::json!("8080:80")).unwrap(),
            Port::new(8080, 80).unwrap()
        );

        for bad in [
            "", "80", "80:", ":80", "0:80", "8080:0", "http", "70000:1", "1:2:3",
        ] {
            assert!(bad.parse::<Port>().is_err(), "{bad:?}");
        }
    }

    /// A mount's spelling round-trips.
    #[test]
    fn a_mount_survives_its_spelling() {
        for spec in [
            "file:///srv/project:/work",
            "file:///srv/project:/work:ro",
            "https://example.com:8080/share:/work/in:ro",
            "file:///srv/odd:name:/work",
        ] {
            let read: MountSpec = spec.parse().expect(spec);
            assert_eq!(read.to_string(), spec);
        }

        // Explicit `rw` is the one spelling that does not round-trip: it is the default.
        let explicit: MountSpec = "file:///srv/project:/work:rw".parse().unwrap();
        assert_eq!(explicit.to_string(), "file:///srv/project:/work");
    }

    /// The three parts a server branches on, read out of one string.
    #[test]
    fn a_mount_says_where_the_tree_is_and_where_it_goes() {
        let mount: MountSpec = "file:///srv/project:/work:ro".parse().unwrap();

        assert_eq!(mount.host_url(), "file:///srv/project");
        assert_eq!(mount.guest_path(), Path::new("/work"));
        assert!(mount.is_read_only());
        assert_eq!(mount.scheme(), "file");
        assert_eq!(mount.file_path(), Some(Path::new("/srv/project")));

        // A scheme with no provider still parses; the build refuses it, not the parser.
        let remote: MountSpec = "s3://bucket/prefix:/work".parse().unwrap();
        assert_eq!(remote.scheme(), "s3");
        assert_eq!(remote.file_path(), None);
        assert!(!remote.is_read_only());
    }

    /// Each refusal names the rule and the offending string.
    #[test]
    fn what_is_not_a_mount_says_which_rule_it_broke() {
        for (spec, why) in [
            ("/srv/project:/work", "a mount URL needs a scheme"),
            (
                "file://:/work",
                "a mount URL needs something after its scheme",
            ),
            (
                "file:///srv/project",
                "a mount needs a guest path to appear at",
            ),
            (
                "file:///srv/project:work",
                "a mount trails an absolute guest path with `ro` or `rw`",
            ),
            (
                "file:///srv/project:/work:rx",
                "a mount trails an absolute guest path with `ro` or `rw`",
            ),
        ] {
            let refused = spec.parse::<MountSpec>().expect_err(spec).to_string();
            assert!(refused.starts_with(why), "{spec}: {refused}");
            assert!(refused.contains(spec), "{spec}: {refused}");
        }

        // The same rules hold for a constructed mount, so every value round-trips.
        assert!(MountSpec::new("file:///srv/project", "work").is_err());
        assert!(MountSpec::new("file:///srv/project", "/wo:rk").is_err());
    }

    /// The wire carries the string and nothing around it.
    #[test]
    fn a_session_names_its_trees_as_strings() {
        let init = InitCall {
            mounts: vec![
                MountSpec::new("file:///srv/project", "/work")
                    .unwrap()
                    .read_only(),
                MountSpec::new("file:///srv/out", "/work/out").unwrap(),
            ],
            ..InitCall::default()
        };

        let wire = bson::serialize_to_bson(&init).unwrap();
        assert_eq!(
            wire,
            bson::bson!({
                "mounts": ["file:///srv/project:/work:ro", "file:///srv/out:/work/out"],
            })
        );
        assert_eq!(bson::deserialize_from_bson::<InitCall>(wire).unwrap(), init);
    }

    /// Each machine member is absent from the frame unless set.
    #[test]
    fn a_session_says_only_the_shape_it_asked_for() {
        let quiet = bson::serialize_to_bson(&InitCall::default()).unwrap();
        assert_eq!(quiet, bson::bson!({}));

        let init = InitCall {
            vcpus: Some(4),
            memory_mib: Some(4096),
            // Explicitly no GPU, distinct from leaving it out.
            gpu: Some(false),
            ..InitCall::default()
        };
        let wire = bson::serialize_to_bson(&init).unwrap();
        assert_eq!(
            wire,
            bson::bson!({ "vcpus": 4, "memory_mib": 4096i64, "gpu": false })
        );
        assert_eq!(bson::deserialize_from_bson::<InitCall>(wire).unwrap(), init);

        let init = InitCall {
            gpu: Some(true),
            gpu_memory_mib: Some(8192),
            ..InitCall::default()
        };
        let wire = bson::serialize_to_bson(&init).unwrap();
        assert_eq!(
            wire,
            bson::bson!({ "gpu": true, "gpu_memory_mib": 8192i64 })
        );
        assert_eq!(bson::deserialize_from_bson::<InitCall>(wire).unwrap(), init);

        let init = InitCall {
            disk_gib: Some(32),
            ..InitCall::default()
        };
        let wire = bson::serialize_to_bson(&init).unwrap();
        assert_eq!(wire, bson::bson!({ "disk_gib": 32i64 }));
        assert_eq!(bson::deserialize_from_bson::<InitCall>(wire).unwrap(), init);
    }
}
