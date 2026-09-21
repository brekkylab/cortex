//! `cortex-uvm-v2-boot` — assemble the micro-VM a host described, and enter it.
//!
//! This never returns. `Vm::enter` hands the process to the VMM, and when the guest shuts
//! down the VMM calls `_exit` — so whatever runs a VM *becomes* a VM and stops being able to
//! do anything else. That is the first reason a boot is a child process: a host has a session
//! to keep answering, and it cannot be the thing that disappears when the guest does.
//!
//! The second is the signature. Creating a VM through Hypervisor.framework needs an
//! entitlement, an entitlement is carried by a code signature, and a signature is read at
//! `exec` — so the process that creates a VM can never be the process that decided to. This
//! binary is signed where it is built and spawned as a file beside the host; what gets signed
//! is therefore this, which links a VMM and a network stack and nothing else, rather than the
//! host, which links an image store, a registry client and a console server.
//!
//! Everything below comes off the command line, which [`contract::BootArgs`] spells out.
//! Nothing is decided here that the host did not already decide — this binary exists to hold
//! a hypervisor, not to have opinions.
//!
//! # The devices, and what each one is for
//!
//! ```text
//! virtio-fs  root           the boot root: the guest binary, the image spec, and the layer
//!                           the guest leaves there on the way back out
//! virtio-fs  cortexctx      the session's context, served straight out of this process
//! virtio-fs  cortexart      its artifacts tree, when it named one
//! virtio-blk /dev/vda       the session's ext4 image — the overlay's upper
//! virtio-blk /dev/vdb       the base image, read-only — the overlay's lower
//! virtio-blk /dev/vdc       `/abin`, read-only, when the session has one
//! virtio-con cortex-…       the console session, the other end of it a socket on the host
//! ```
//!
//! The disks are attached in that order because attach order is what fixes the guest names,
//! and the guest is told which is which by name — see [`contract::GUEST_UPPER_DEV`].
//!
//! A tree is a **host directory**, shared over virtio-fs like any other. Whatever it is made
//! of was realized and mounted on the host before this process started, so nothing in here
//! knows or asks: it attaches a path. Where it lands in the guest is a constant per role —
//! see [`contract::CONTEXT_PATH`].

mod net;

// The cross-process contract, in the library all three halves of this take. Imported at
// the root so that the rest of the crate names it `crate::contract`, the way it would a
// module of its own.
pub(crate) use cortex_uvm_v2_common::contract;

use std::{
    convert::Infallible,
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::{Path, PathBuf},
};

use contract::{
    ABIN_ENV, ABIN_PATH, ABIN_TAG, ARTIFACTS_ENV, ARTIFACTS_PATH, ARTIFACTS_TAG, BaseFormat,
    BootArgs, CONTEXT_ENV,
    CONTEXT_PATH, CONTEXT_TAG, GUEST_ABIN_DEV, GUEST_BIN_PATH, GUEST_LOWER_DEV, GUEST_UPPER_DEV,
    LOWER_ENV, Network, PORT_NAME, UPPER_ENV,
};
use msb_krun::{DiskImageFormat, VmBuilder};

/// Guest vCPUs when nothing says otherwise. Two rather than one because a command and the
/// agent collecting its output are two things wanting the processor at once, and one vCPU
/// turns that into a queue.
const DEFAULT_VCPUS: u8 = 2;

/// Guest memory in MiB when nothing says otherwise. Enough for a package install, which is
/// the first thing anyone does in a sandbox.
const DEFAULT_MEMORY_MIB: u32 = 2048;

/// Not async, and not multi-threaded. This process assembles a VM and hands itself to the
/// VMM; everything it waits for after that it waits for by not existing. The stack a session
/// with a network gets has a runtime of its own, started in [`net`] and held until the VM is
/// gone.
fn main() -> std::process::ExitCode {
    match BootArgs::parse(std::env::args_os().skip(1)).and_then(enter) {
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

fn enter(args: BootArgs) -> anyhow::Result<Infallible> {
    // The console port is a descriptor, and this is where it comes from: one connection back
    // to the process that spawned this one. Held for the length of this function, which is
    // the length of the process.
    let channel = UnixStream::connect(&args.channel)
        .map_err(|e| anyhow::anyhow!("connecting to the console channel: {e}"))?;
    let port = channel.as_raw_fd();

    let kernel = kernel()?;

    let lower_format = match args.base_format {
        BaseFormat::Raw => DiskImageFormat::Raw,
        BaseFormat::Vmdk => DiskImageFormat::Vmdk,
    };
    let mut builder = VmBuilder::new()
        .machine(|m| {
            m.vcpus(args.vcpus.unwrap_or(DEFAULT_VCPUS))
                .memory_mib(args.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB) as usize)
        })
        .kernel(|k| k.krunfw_path(&kernel))
        .fs(|fs| fs.root(&args.boot_root))
        // Attach order is what fixes the names: the session's own image is `/dev/vda` and the
        // base is `/dev/vdb`, which is the promise the guest mounts against.
        .disk(|d| d.path(&args.session).format(DiskImageFormat::Raw))
        .disk(|d| d.path(&args.base).read_only(true).format(lower_format))
        // The same descriptor both ways: a socket is bidirectional, and the port the guest
        // opens is one thing rather than a pair. The console beside it is the guest's own,
        // and goes to a file so that a boot nobody could talk to can still be read.
        .console(|c| {
            let c = match &args.console {
                Some(at) => c.output(at),
                None => c,
            };
            c.port(PORT_NAME, port, port)
        });

    // `/abin`, third and so `/dev/vdc`. Read-only **at the device**, which is the whole reason
    // an image of the executables is a disk and not a share: a virtio-fs share has no such
    // option, and the guest is root inside itself, so a guest-side mount flag would be a guard
    // rail rather than a boundary.
    //
    // A directory is the other thing `--abin` can name, and it has no image to attach — so it
    // is shared like the session's trees are, below, and the guest is told a tag instead of a
    // device. That form gives up the boundary above, which is why what decides between them is
    // a fact about the path rather than anything this process chooses.
    let abin_dir = args.abin.as_deref().filter(|abin| abin.is_dir());
    if let Some(abin) = args.abin.as_deref().filter(|_| abin_dir.is_none()) {
        builder = builder.disk(|d| d.path(abin).read_only(true).format(DiskImageFormat::Raw));
    }

    // The network, when the session asked for one. The stack, its runtime and the policy it
    // enforces all live in this process, and all three have to outlive `enter` below — which
    // never returns, so the guard is held to the end of the function that does not end.
    let mut stack = match args.network {
        Network::Disabled => None,
        reach => Some(net::start(reach, &args.host_ports)?),
    };
    if let Some(stack) = &mut stack {
        let (mac, backend) = stack.device();
        builder = builder.net(move |n| n.mac(mac).custom(backend));
    }

    // One device per tree the session named, each under its own tag, and both the same
    // work: this has no opinion about what a tree is for, and the guest is told which is
    // which by the name it arrives under.
    let mut shares: Vec<(&str, String)> = Vec::new();
    for (flag, env, tag, at, path) in [
        (
            "--context",
            CONTEXT_ENV,
            CONTEXT_TAG,
            CONTEXT_PATH,
            args.context.as_deref(),
        ),
        (
            "--artifacts",
            ARTIFACTS_ENV,
            ARTIFACTS_TAG,
            ARTIFACTS_PATH,
            args.artifacts.as_deref(),
        ),
        ("--abin", ABIN_ENV, ABIN_TAG, ABIN_PATH, abin_dir),
    ] {
        let Some(path) = path else { continue };
        // UTF-8 because it has to be written into that tree's env as `tag:path` and read back
        // by the guest — a directory with no string form is one the two ends could not agree
        // on, and this is the last place that can say so.
        anyhow::ensure!(
            path.is_absolute(),
            "{flag} has to be an absolute path, and is {}",
            path.display()
        );
        let path: &str = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("{flag} is not utf-8: {}", path.display()))?;

        builder = builder.fs(|fs| fs.tag(tag).path(Path::new(path)));
        shares.push((env, format!("{tag}:{at}")));
    }

    // What the stack wants the guest to know: its address, its gateway, its resolver. Passed
    // through as the stack spelled them.
    let guest_net = stack.as_ref().map(|s| s.guest_env()).unwrap_or_default();

    let vm = builder
        .exec(|e| {
            let e = e
                .path(GUEST_BIN_PATH)
                .env(LOWER_ENV, GUEST_LOWER_DEV)
                .env(UPPER_ENV, GUEST_UPPER_DEV);
            let e = shares.iter().fold(e, |e, (env, share)| e.env(*env, share));
            // Only for the disk form: a shared `/abin` is in `shares` above, and its tag has
            // already gone into the same variable.
            let e = match &args.abin {
                Some(_) if abin_dir.is_none() => e.env(ABIN_ENV, GUEST_ABIN_DEV),
                _ => e,
            };
            guest_net
                .iter()
                .fold(e, |e, (name, value)| e.env(name, value))
        })
        .build()?;

    Ok(vm.enter()?)
}

/// The libkrunfw to load a guest kernel out of: the one `CORTEX_UVM_KERNEL` names, or the one
/// this host has installed.
///
/// Neither downloaded nor shipped. libkrun `dlopen`s this library, and a `dlopen`ed library
/// has to be signed compatibly with the process loading it — this one. So which copy is usable
/// is a property of this binary and its installation, which is why the override is read here
/// rather than anywhere a server could have answered it.
///
/// Resolved before the VM is built, because a name that is missing or wrong otherwise fails
/// inside the VMM, well past the point where a message saying what to install is easy to
/// give.
fn kernel() -> anyhow::Result<PathBuf> {
    if let Some(named) = std::env::var_os("CORTEX_UVM_KERNEL") {
        return Ok(PathBuf::from(named));
    }

    // Both spellings of the name: the versioned one a release ships, and the plain one a
    // package manager symlinks.
    let (versioned, plain) = match std::env::consts::OS {
        "macos" => ("libkrunfw.5.dylib", "libkrunfw.dylib"),
        _ => ("libkrunfw.so.5", "libkrunfw.so"),
    };
    let mut looked = Vec::new();
    if let Some(user) = std::env::var_os("HOME") {
        looked.push(PathBuf::from(&user).join(".microsandbox/lib"));
    }
    looked.extend(
        ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"]
            .into_iter()
            .map(PathBuf::from),
    );
    looked
        .into_iter()
        .flat_map(|dir| [dir.join(versioned), dir.join(plain)])
        .find(|at| at.exists())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no libkrunfw found. Install it (`brew install libkrunfw`, or microsandbox), \
                 or set CORTEX_UVM_KERNEL"
            )
        })
}
