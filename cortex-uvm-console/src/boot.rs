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
//! virtio-blk /dev/vdb     the base EROFS image, read-only — the overlay's lower
//! virtio-con cortex-…     the console session, the other end of it a socket on the host
//! ```
//!
//! The two disks are attached in that order because attach order is what fixes the guest
//! names, and the guest is told which is which by name — see
//! [`GUEST_UPPER_DEV`](crate::contract::GUEST_UPPER_DEV).
//!
//! The workspace is the one device that is not a file. cortex realizes it as a
//! [`Posix`] over a [`WorkFs`], which `msb_krun` drives as a `DynFileSystem`, so
//! every request the guest kernel makes is answered by this process — no daemon and no host
//! mount.
//!
//! What is behind it is whatever the client declared: a `WorkspaceSpec` arrives on a file
//! this process reads once and unlinks, and every volume kind cortex can realize is a
//! volume kind a guest can see. Where the tree lands is not negotiable — it is
//! [`GUEST_WORKSPACE_ROOT`](crate::contract::GUEST_WORKSPACE_ROOT), a constant, because a
//! spec's own mount paths are relative to the workspace root and one spec has to mean one
//! namespace on either backend.
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
use std::path::{Path, PathBuf};

use cortex::fs::{Posix, WorkFs};
use msb_krun::{DiskImageFormat, DynFileSystem, VmBuilder};

use crate::contract::{
    BASE_IMAGE_ENV, BOOT_ROOT_ENV, CHANNEL_ENV, GUEST_BIN_PATH, GUEST_LOWER_DEV, GUEST_UPPER_DEV,
    GUEST_WORKSPACE_ROOT, KERNEL_ENV, LOWER_ENV, MEMORY_ENV, PORT_NAME, SESSION_IMAGE_ENV,
    SHARE_ENV, SPEC_ENV, UPPER_ENV, VCPUS_ENV, WORKSPACE_TAG,
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

/// Read a namespace off disk and unlink it.
///
/// Split out from [`workspace`] because it is the part with an answer a test can check.
/// The unlink is not cleanup: the file is credentials, this is the last reader, and the
/// sooner it stops existing the smaller the window in which anything else could read it.
fn read_spec(path: &Path) -> anyhow::Result<WorkFs> {
    let encoded = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("reading the namespace from {}: {e}", path.display()))?;
    // Before parsing, not after: a spec this build cannot make sense of is still a spec that
    // should not be sitting on disk.
    let _ = std::fs::remove_file(path);

    let spec: cortex::fs::WorkspaceSpec = bson::deserialize_from_slice(&encoded)
        .map_err(|e| anyhow::anyhow!("parsing the namespace: {e}"))?;
    WorkFs::from_spec(&spec).map_err(|e| anyhow::anyhow!("realizing the namespace: {e}"))
}

/// The cortex workspace to project into the guest, and where it lands.
///
/// One virtio-fs device for the whole tree. What is under it is a [`WorkFs`]'s
/// business — however many volumes, of whatever kind, mounted wherever the spec says — and
/// the guest sees whatever that tree is, which is the same tree a host FUSE mount built from
/// the same spec would show. That sameness is the point of the spec existing.
fn workspace() -> anyhow::Result<Option<(Box<dyn DynFileSystem + Send + Sync>, String)>> {
    let Some(path) = std::env::var_os(SPEC_ENV) else {
        return Ok(None);
    };
    let ws = read_spec(Path::new(&path))?;
    Ok(Some((
        Box::new(Posix::new(ws)),
        GUEST_WORKSPACE_ROOT.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A spec on disk becomes the tree it describes, and the file is gone afterwards.
    ///
    /// `async` only because `Mountable` is: `read_spec` itself is not, and neither is
    /// anything else this file does with a spec. Asking the tree a question is the one step
    /// that awaits.
    #[tokio::test]
    async fn a_spec_file_becomes_a_workspace_and_is_unlinked() {
        use cortex::fs::{Mountable as _, VolumeSpec, WorkspaceSpec};

        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("hello.txt"), b"hi").unwrap();

        let spec = WorkspaceSpec::default().mount(
            "work",
            VolumeSpec::Local {
                host: host.path().to_path_buf(),
            },
        );
        let path = std::env::temp_dir().join(format!("spec-test-{}.bson", std::process::id()));
        std::fs::write(&path, bson::serialize_to_vec(&spec).unwrap()).unwrap();

        let ws = read_spec(&path).expect("builds the workspace the spec describes");

        // The tree is the spec's, asked through the `Mountable` surface rather than through
        // a mount — no VM, no virtio-fs, just the object the boot child hands libkrun.
        assert!(ws.stat(Path::new("work/hello.txt")).await.is_ok());

        assert!(
            !path.exists(),
            "the spec is credentials, and a reader that leaves it on disk is the leak this \
             file's whole shape exists to avoid"
        );
    }

    /// A spec file that is not there at all is a hard failure, not an empty namespace.
    ///
    /// Silence would mean a session whose client declared volumes gets a guest with none,
    /// and no way to tell. Declaring nothing is spelled by `SPEC_ENV` being unset, which
    /// never reaches here.
    #[test]
    fn a_missing_spec_file_is_an_error() {
        let missing = std::env::temp_dir().join("cortex-uvm-no-such-spec.bson");
        let _ = std::fs::remove_file(&missing);
        assert!(read_spec(&missing).is_err());
    }
}
