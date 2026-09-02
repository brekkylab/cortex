//! `cortex-uvm-boot` — assemble the micro-VM a console server described, and enter it.
//!
//! This never returns. `Vm::enter` hands the process to the VMM, and when the guest shuts
//! down the VMM calls `_exit` — so whatever runs a VM *becomes* a VM and stops being able
//! to do anything else. That is the whole reason a boot is a child process: a console
//! server has a session to keep answering, and it cannot be the thing that disappears
//! when the guest does.
//!
//! Everything below comes out of the environment, which this crate's own library half
//! spells out. Nothing is decided here that the server did not already decide — this binary
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
//! [`GUEST_UPPER_DEV`](cortex_uvm_boot::GUEST_UPPER_DEV).
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
//! A virtio-net device, when the session asked for one, whose far end is a userspace stack in
//! *this* process — see [`net`]. A session that asked for nothing gets no device, which is a
//! stronger air-gap than any policy over a device that is there.
//!
//! Not the transparent-socket fallback either way: `enable_inet_hijack` is left alone, so
//! libkrun's own rule applies — a guest with no virtio-net and no hijack reaches nothing.

mod net;

use std::{
    convert::Infallible,
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::Path,
};

use msb_krun::{DiskImageFormat, VmBuilder};

use cortex_uvm_boot::{
    ABIN_ENV, BaseFormat, BootArgs, GUEST_ABIN_DEV, GUEST_BIN_PATH, GUEST_LOWER_DEV,
    GUEST_UPPER_DEV, LOWER_ENV, Network, PORT_NAME, SHARE_ENV, UPPER_ENV, WORKFS_TAG,
};

/// Guest vCPUs when nothing says otherwise. Two rather than one because a command that
/// delegates has a shim waiting on a socket while the command that ran it is still
/// running, and one vCPU turns that into a queue.
const DEFAULT_VCPUS: u8 = 2;

/// Guest memory in MiB when nothing says otherwise. Enough for a package install, which
/// is the first thing anyone does in a sandbox.
const DEFAULT_MEMORY_MIB: u32 = 2048;

/// Build the VM, and enter it.
///
/// Everything comes off the command line, which this crate's library half spells as
/// [`BootArgs`]. Nothing is decided here that the server did not already decide: this binary
/// exists to hold a hypervisor, not to have opinions.
fn run(args: BootArgs) -> anyhow::Result<Infallible> {
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

    // `/abin`, third and so `/dev/vdc`. Read-only **at the device**, which is the whole
    // reason it is a disk and not a share: a virtio-fs share has no such option, and the
    // guest is root inside itself, so a guest-side mount flag would be a guard rail rather
    // than a boundary.
    //
    // Raw, always: `/abin` is cortex's own executables and no others, which is one layer of
    // the store attached as it stands rather than anything stitched into a descriptor.
    if let Some(abin) = &args.abin {
        builder = builder.disk(|d| d.path(abin).read_only(true).format(DiskImageFormat::Raw));
    }

    // The network, when the session asked for one. Everything about it lives in this process:
    // the stack, its runtime, and the policy it enforces — and all three have to outlive
    // `enter` below, which never returns, so the guard is held to the end of the function that
    // does not end.
    let mut stack = match args.network {
        Network::Disabled => None,
        reach => Some(net::start(reach, &args.host_ports)?),
    };
    if let Some(stack) = &mut stack {
        let (mac, backend) = stack.device();
        builder = builder.net(move |n| n.mac(mac).custom(backend));
    }

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

    // What the stack wants the guest to know: its address, its gateway, its resolver. Passed
    // through as the stack spelled them — the names are `microsandbox-network`'s own, and this
    // process is not a party to what they mean. The guest reads them; see its `net` module.
    let guest_net = stack.as_ref().map(|s| s.guest_env()).unwrap_or_default();

    let vm = builder
        .exec(|e| {
            let e = e
                .path(GUEST_BIN_PATH)
                .env(LOWER_ENV, GUEST_LOWER_DEV)
                .env(UPPER_ENV, GUEST_UPPER_DEV);
            let e = match &share {
                Some(share) => e.env(SHARE_ENV, share),
                None => e,
            };
            let e = match &args.abin {
                Some(_) => e.env(ABIN_ENV, GUEST_ABIN_DEV),
                None => e,
            };
            guest_net
                .iter()
                .fold(e, |e, (name, value)| e.env(name, value))
        })
        .build()?;

    Ok(vm.enter()?)
}

/// Not async, and not multi-threaded. This process assembles a VM and hands itself to the
/// VMM; everything it waits for after that it waits for by not existing.
fn main() -> std::process::ExitCode {
    match BootArgs::parse(std::env::args_os().skip(1)).and_then(run) {
        // `enter` only returns on success by not returning at all: the `Ok` holds an
        // `Infallible`, and the empty match is what says so.
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            // The shell's code for "found it, could not run it", which is what a boot that
            // could not become a VM is.
            std::process::ExitCode::from(126)
        }
    }
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
