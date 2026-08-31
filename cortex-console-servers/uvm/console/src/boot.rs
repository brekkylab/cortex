//! The boot role: assemble the micro-VM and enter it.
//!
//! This never returns. `Vm::enter` hands the process to the VMM, and when the guest shuts
//! down the VMM calls `_exit` — so whatever runs a VM *becomes* a VM and stops being able
//! to do anything else. That is the whole reason a boot is a child process: a console
//! server has a session to keep answering, and it cannot be the thing that disappears
//! when the guest does.
//!
//! Everything below comes out of the environment, which [`contract`](crate::contract)
//! spells out. Nothing is decided here that the server did not already decide — this role
//! exists to hold a hypervisor, not to have opinions.
//!
//! # The devices, and what each one is for
//!
//! ```text
//! virtio-fs  root         the boot root, holding the guest binary and nothing else
//! virtio-fs  cortexws     a cortex WorkFs, served straight out of this process
//! virtio-blk /dev/vda     the session's ext4 image — the overlay's upper
//! virtio-blk /dev/vdb     the base image, read-only — the overlay's lower
//! virtio-con cortex-…     the console session, the other end of it a socket on the host
//! ```
//!
//! The two disks are attached in that order because attach order is what fixes the guest
//! names, and the guest is told which is which by name — see
//! [`GUEST_UPPER_DEV`](crate::contract::GUEST_UPPER_DEV).
//!
//! The base is attached in the format the server said ([`base_format`]): raw for an image
//! encoded from a tarball, VMDK for a registry pull's, whose descriptor stitches one disk out
//! of a layer per file. The guest mounts either as `erofs` — the stitching is a host-side
//! detail that stops at the block layer.
//!
//! The tree is a **host directory**, shared over virtio-fs like any other. Whatever it is
//! made of — a cortex `WorkFs` of several stores, a plain project directory — was realized
//! and mounted on the host before this process started, so nothing in here knows or asks:
//! it attaches a path.
//!
//! Where it lands in the guest is **that same path**, and that is the one decision worth
//! reading here. A constant of our own (`/workspace`, say) would mean a directory has two
//! names, one per side of the hypervisor, and every path crossing between them would have
//! to be rewritten — a `cwd` on the way out, a `read` on the way in, an argv nobody could
//! rewrite safely. Mounting it where the host already has it makes the two spellings one
//! string, and the translation problem does not exist.
//!
//! # Networking
//!
//! None. No virtio-net device is attached and the transparent-socket fallback is off by
//! default, so the guest cannot reach anything — not the host, not a LAN, not the
//! internet. A session that needs egress needs a network device and a policy to go with
//! it, and neither is something to add without a way for a caller to say what it wants.

use std::{
    convert::Infallible,
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
};

use msb_krun::{DiskImageFormat, VmBuilder};

use crate::contract::{
    BaseFormat, BootArgs, GUEST_BIN_PATH, GUEST_LOWER_DEV, GUEST_UPPER_DEV, LOWER_ENV, PORT_NAME,
    SHARE_ENV, UPPER_ENV, WORKFS_TAG,
};

/// Guest vCPUs when nothing says otherwise. Two rather than one because a command that
/// delegates has a shim waiting on a socket while the command that ran it is still
/// running, and one vCPU turns that into a queue.
const DEFAULT_VCPUS: u8 = 2;

/// Guest memory in MiB when nothing says otherwise. Enough for a package install, which
/// is the first thing anyone does in a sandbox.
const DEFAULT_MEMORY_MIB: u32 = 2048;

/// Build the VM this process was started to be, and enter it.
///
/// Everything comes off the command line, which this crate's own [`BootArgs`] spells. Nothing
/// is decided here that the server did not already decide — this role exists to hold a
/// hypervisor, not to have opinions.
pub fn run(args: BootArgs) -> anyhow::Result<Infallible> {
    // The console port is a descriptor, and this is where it comes from: one connection
    // back to the server that spawned us. Held for the length of this function, which is
    // the length of the process — `enter` below does not return.
    let channel = UnixStream::connect(&args.channel)
        .map_err(|e| anyhow::anyhow!("connecting to the console channel: {e}"))?;
    let port = channel.as_raw_fd();

    let lower_format = match args.base_format {
        BaseFormat::Raw => DiskImageFormat::Raw,
        BaseFormat::Vmdk => DiskImageFormat::Vmdk,
    };
    let mut builder = VmBuilder::new()
        .machine(|m| {
            m.vcpus(args.vcpus.unwrap_or(DEFAULT_VCPUS))
                .memory_mib(args.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB) as usize)
        })
        .kernel(|k| k.krunfw_path(&args.kernel))
        .fs(|fs| fs.root(&args.boot_root))
        .disk(|d| d.path(&args.session).format(DiskImageFormat::Raw))
        .disk(|d| d.path(&args.base).read_only(true).format(lower_format))
        // The same descriptor both ways: a socket is bidirectional, and the port the
        // guest opens is one thing rather than a pair.
        .console(|c| c.port(PORT_NAME, port, port));

    // Attached here and named to the guest below, because the tag is one agreement in two
    // places: a device configuration and a `mount -t virtiofs`. The path is the second
    // agreement, and it is the host's own — see the module docs on why the guest mounts it
    // where this host has it.
    let share = match workfs(args.workfs.as_deref())? {
        Some(path) => {
            builder = builder.fs(|fs| fs.tag(WORKFS_TAG).path(&path));
            Some(format!("{WORKFS_TAG}:{path}"))
        }
        None => None,
    };

    let vm = builder
        .exec(|e| {
            let e = e
                .path(GUEST_BIN_PATH)
                .env(LOWER_ENV, GUEST_LOWER_DEV)
                .env(UPPER_ENV, GUEST_UPPER_DEV);
            match &share {
                Some(share) => e.env(SHARE_ENV, share),
                None => e,
            }
        })
        .build()?;

    Ok(vm.enter()?)
}

/// The host directory to share, and `None` for a session that declared no tree.
///
/// UTF-8 because it has to be written into [`SHARE_ENV`] as `tag:path` and read back by the
/// guest — a directory with no string form is one the two ends could not agree on, and this
/// is the last place that can say so. Which is why it is checked here and not in
/// [`BootArgs::parse`]: everything else there is a path this process only ever opens.
fn workfs(path: Option<&Path>) -> anyhow::Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    anyhow::ensure!(
        path.is_absolute(),
        "--workfs has to be an absolute path, and is {}",
        path.display()
    );
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--workfs is not utf-8: {}", path.display()))?;
    Ok(Some(path.to_string()))
}
