//! What a boot tells the guest, and what a boot child is told.
//!
//! Two cross-process contracts happen to be the same handful of strings, so they live
//! together:
//!
//! - **this process to the boot child** ([`KERNEL_ENV`] and the rest). The child is a copy
//!   of this binary in the [`Boot`](crate::Role::Boot) role, so both ends are this crate
//!   and nothing here can drift.
//! - **this process to the guest** ([`LOWER_ENV`], [`UPPER_ENV`], [`SHARE_ENV`],
//!   [`GUEST_BIN_PATH`], [`PORT_NAME`], [`HANDSHAKE`]). The other end of that one is
//!   `cortex-uvm-guest`'s `contract` module, which **has to change with this file** — the
//!   two crates are built for different targets, so neither can depend on the other and
//!   the compiler will not notice a mismatch.
//!
//! Where the tree lands inside the guest is **the host's own path for it**, carried in
//! [`SHARE_ENV`] like any other share. Not a constant of this crate's choosing: one spelling
//! on both sides of the hypervisor is what lets a `cwd` the guest reports be a path the
//! client can open, with nothing in the middle translating and nothing to disagree about.
//!
//! The environment is the channel because it is the only one libkrun's `exec` has that
//! survives: an argument becomes part of the guest's kernel command line, which is
//! size-limited and rejects a newline. Nothing that could grow is here — the session
//! itself arrives on [`PORT_NAME`] as protocol frames.

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

/// The libkrunfw kernel the child boots.
pub const KERNEL_ENV: &str = "CORTEX_UVM_KERNEL";

/// The boot root: a directory served as the guest's virtio-fs root, holding the guest
/// binary and nothing else.
pub const BOOT_ROOT_ENV: &str = "CORTEX_UVM_BOOT_ROOT";

/// Where the child connects to reach this process. One socket, one connection, and its
/// descriptor becomes the guest's console port.
pub const CHANNEL_ENV: &str = "CORTEX_UVM_CHANNEL";

/// Guest vCPU count, if the caller overrode it.
pub const VCPUS_ENV: &str = "CORTEX_UVM_VCPUS";

/// Guest memory in MiB, if the caller overrode it.
pub const MEMORY_ENV: &str = "CORTEX_UVM_MEMORY_MIB";

/// The host directory to put in front of the guest — the tree the session works in.
///
/// A path and nothing else, because that is all a tree is now: whatever it is made of was
/// realized on the host before this, and what a guest gets is the directory it was mounted
/// at. Nothing secret travels here, which is what lets it be an environment value at all.
///
/// Unset means a session that declared no tree, which is a session.
pub const WORKFS_ENV: &str = "CORTEX_UVM_WORKFS";

/// The read-only base image, as a host path (the child) — and as a guest block device
/// (the guest). Both ends of the overlay are named twice for that reason: one side has a
/// file, the other has a device.
pub const BASE_IMAGE_ENV: &str = "CORTEX_UVM_BASE_IMAGE";

/// How the base image is laid out on the host, spelled as one of [`BaseFormat`]'s names.
///
/// A boot is told rather than left to guess from a path: a registry pull's base is a VMDK
/// descriptor stitching per-layer EROFS blobs, where a tarball's is a single raw EROFS, and
/// the guest mounts both as `erofs` — the difference is only what the VMM has to read.
///
/// Also the caller's knob for an image built elsewhere, alongside [`BASE_IMAGE_ENV`].
pub const BASE_FORMAT_ENV: &str = "CORTEX_UVM_BASE_FORMAT";

/// The session's writable image, as a host path.
pub const SESSION_IMAGE_ENV: &str = "CORTEX_UVM_SESSION_IMAGE";

/// The overlay's lower, as the guest sees it. Attach order is what fixes the names, so
/// this is a promise [`crate::boot`] keeps rather than something either end computes.
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
            other => anyhow::bail!("{BASE_FORMAT_ENV}: {other} is not `raw` or `vmdk`"),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
