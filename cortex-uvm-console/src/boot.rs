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
//! virtio-fs  cortexws     a cortex Workspace, served straight out of this process
//! virtio-blk /dev/vda     the session's ext4 image — the overlay's upper
//! virtio-blk /dev/vdb     the base EROFS image, read-only — the overlay's lower
//! virtio-con cortex-…     the console session, the other end of it a socket on the host
//! ```
//!
//! The two disks are attached in that order because attach order is what fixes the guest
//! names, and the guest is told which is which by name — see
//! [`GUEST_UPPER_DEV`](crate::contract::GUEST_UPPER_DEV).
//!
//! The workspace is the one device that is not a file. cortex realizes it as a
//! [`PosixFs`] over a [`Workspace`], which `msb_krun` drives as a `DynFileSystem`, so
//! every request the guest kernel makes is answered by this process — no daemon, no host
//! mount, and no reason for a volume to be a local directory beyond that being what a
//! console server is handed today.
//!
//! # Networking
//!
//! None. No virtio-net device is attached and the transparent-socket fallback is off by
//! default, so the guest cannot reach anything — not the host, not a LAN, not the
//! internet. A session that needs egress needs a network device and a policy to go with
//! it, and neither is something to add without a way for a caller to say what it wants.

use std::convert::Infallible;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use cortex::volume::{PassthroughVolume, PosixFs, Workspace};
use msb_krun::{DiskImageFormat, DynFileSystem, VmBuilder};

use crate::contract::{
    BASE_IMAGE_ENV, BOOT_ROOT_ENV, CHANNEL_ENV, GUEST_BIN_PATH, GUEST_LOWER_DEV, GUEST_UPPER_DEV,
    KERNEL_ENV, LOWER_ENV, MEMORY_ENV, PORT_NAME, SESSION_IMAGE_ENV, SHARE_ENV, UPPER_ENV,
    VCPUS_ENV, WORKSPACE_ENV, WORKSPACE_TAG,
};

/// Guest vCPUs when nothing says otherwise. Two rather than one because a command that
/// delegates has a shim waiting on a socket while the command that ran it is still
/// running, and one vCPU turns that into a queue.
const DEFAULT_VCPUS: u8 = 2;

/// Guest memory in MiB when nothing says otherwise. Enough for a package install, which
/// is the first thing anyone does in a sandbox.
const DEFAULT_MEMORY_MIB: u32 = 2048;

/// Build the VM this process was started to be, and enter it.
pub fn run() -> anyhow::Result<Infallible> {
    let kernel = required(KERNEL_ENV)?;
    let boot_root = required(BOOT_ROOT_ENV)?;
    let upper = required(SESSION_IMAGE_ENV)?;
    let lower = required(BASE_IMAGE_ENV)?;
    let channel = required(CHANNEL_ENV)?;

    // The console port is a descriptor, and this is where it comes from: one connection
    // back to the server that spawned us. Held for the length of this function, which is
    // the length of the process — `enter` below does not return.
    let channel = UnixStream::connect(&channel)
        .map_err(|e| anyhow::anyhow!("connecting to the console channel: {e}"))?;
    let port = channel.as_raw_fd();

    let mut builder = VmBuilder::new()
        .machine(|m| {
            m.vcpus(number(VCPUS_ENV).unwrap_or(DEFAULT_VCPUS))
                .memory_mib(number(MEMORY_ENV).unwrap_or(DEFAULT_MEMORY_MIB) as usize)
        })
        .kernel(|k| k.krunfw_path(&kernel))
        .fs(|fs| fs.root(&boot_root))
        .disk(|d| d.path(&upper).format(DiskImageFormat::Raw))
        .disk(|d| d.path(&lower).read_only(true).format(DiskImageFormat::Raw))
        // The same descriptor both ways: a socket is bidirectional, and the port the
        // guest opens is one thing rather than a pair.
        .console(|c| c.port(PORT_NAME, port, port));

    // Attached here and named to the guest below, because the tag is one agreement in two
    // places: a device configuration and a `mount -t virtiofs`.
    let share = match workspace()? {
        Some((backend, guest_root)) => {
            builder = builder.fs(move |fs| fs.tag(WORKSPACE_TAG).custom(backend));
            Some(format!("{WORKSPACE_TAG}:{guest_root}"))
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

/// The cortex workspace to project into the guest, and where it lands.
///
/// One virtio-fs device for the whole tree. What is under it is a [`Workspace`]'s
/// business — one volume today, mounted at its root — and the guest sees whatever that
/// tree is, which is the same tree a host FUSE mount built from the same workspace would
/// show.
fn workspace() -> anyhow::Result<Option<(Box<dyn DynFileSystem + Send + Sync>, String)>> {
    let Ok(spec) = std::env::var(WORKSPACE_ENV) else {
        return Ok(None);
    };
    let Some((host, guest_root)) = spec.split_once(':') else {
        anyhow::bail!("{WORKSPACE_ENV} is `{spec}`, which is not `/host/path:/guest/path`");
    };
    anyhow::ensure!(
        guest_root.starts_with('/'),
        "{WORKSPACE_ENV} names `{guest_root}` in the guest, which is not an absolute path"
    );

    // Mounted at the workspace's own root: the volume *is* the tree, so the guest's
    // mountpoint is where it begins.
    let workspace = Workspace::new()
        .try_with_mount("", PassthroughVolume::new(host))
        .map_err(|e| anyhow::anyhow!("building the workspace over {host}: {e}"))?;

    Ok(Some((
        Box::new(PosixFs::new(workspace)),
        guest_root.to_string(),
    )))
}

fn required(key: &str) -> anyhow::Result<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("{key} is unset — a boot is started by a console server"))
}

/// An override read out of the environment, ignoring anything that is not a number: a
/// caller who typed nonsense gets the default and a guest that boots.
fn number<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.parse().ok()
}
