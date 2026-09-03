//! Turning a booted kernel into somewhere commands can run.
//!
//! libkrun's `/init.krun` mounts the boot root over virtio-fs and execs this binary. What
//! it hands over is not yet a system: there is no `/proc`, no `/dev`, nothing writable,
//! and the root it did mount holds one file — this one. Everything below is what stands
//! between that and a root where `sh -c 'echo hi > out.txt'` means what it says.
//!
//! # Every mount here is the syscall
//!
//! Not the image's `mount` binary, and not because spawning one would be slower. On most
//! images `mount` is util-linux's, which drives a mount through the `fsopen`/`fsmount`
//! API — and the libkrun kernel rejects that, so the command fails on an image where the
//! syscall underneath it would have worked. Busybox's `mount` calls `mount(2)` and works,
//! which is exactly why an Alpine guest looks fine and a Debian one does not.
//!
//! Calling the syscall directly is the version that does not depend on which of those the
//! base image ships.
//!
//! # The root is an overlay, and that is what makes a session
//!
//! ```text
//! /dev/vdb  erofs, read-only   the base image ─┐
//!                                              ├─ overlay ── pivot_root ──► /
//! /dev/vda  ext4,  writable    this session   ─┘
//! ```
//!
//! Two block devices rather than a writable virtio-fs root, because overlayfs cannot
//! copy up from a virtio-fs lower — and without copy-up, editing any file the image
//! shipped fails. Two devices also make the split exactly the thing the host wants: the
//! base is shared and never written, the upper is this session's and nobody else's, and
//! throwing a session away is deleting one file on the host.
//!
//! [`mount_root`] does this before anything else needs a filesystem. A boot that names
//! neither device runs on the boot root as it is, which is a guest with one file in it —
//! useful only for finding out why a boot failed.
//!
//! # And then the boot root is gone
//!
//! `pivot_root` detaches the old root, so the two files the host left there stop being
//! reachable by any path. This binary does not care — it is already in memory, and nothing
//! after the pivot opens it again — but the [`ImageSpec`] beside it would: so
//! [`image_spec`] reads it before the pivot, and what survives is a value rather than a
//! file.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::ptr;

use crate::contract::{IMAGE_SPEC_PATH, ImageSpec, LOWER_ENV, PORT_NAME, SHARE_ENV, UPPER_ENV};

/// How long to wait for the virtio-console port to appear. The device is probed while
/// this code is mounting, so it is normally there already; the wait is for the boot where
/// it is not yet.
const PORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const PORT_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Build the root, mount what the session was given, and open the channel the host is
/// waiting on.
///
/// Returns the port and what the base image said about running things in it — the two things
/// the agent needs from here and cannot get for itself, the second because it is a file in a
/// root that no longer exists by the time the agent runs.
///
/// Where the tree landed is **not** returned, and that is the point: the agent hears it in
/// the `init` the host replays, as a `file://` URL naming the same absolute path the host
/// spells it with — one fact from one place. This function only has to put the tree where
/// that URL says, which the boot arranged by naming the share after the host's own
/// directory.
///
/// The order is the only one that works: pseudo-filesystems first because `/proc` is how
/// this binary finds itself and `/dev` is where the block devices are, the spec next because
/// the overlay is about to replace the root it is in, then the overlay, then the share, then
/// the port.
pub fn prepare() -> anyhow::Result<(File, ImageSpec)> {
    mount_pseudo();

    let image = image_spec()?;

    if let (Ok(lower), Ok(upper)) = (std::env::var(LOWER_ENV), std::env::var(UPPER_ENV)) {
        mount_root(&lower, &upper)?;
        // The overlay's `/proc`, `/dev` and `/sys` are the base image's empty
        // directories: the mounts from before the pivot went with the old root.
        mount_pseudo();
    }

    // After the pivot, because `/etc/resolv.conf` has to land on the root the commands will
    // see rather than on the one about to be detached. Does nothing when the boot attached no
    // network, which is the default.
    crate::net::configure()?;

    // Where a command runs is the session's, and the agent sets it per command — but this
    // process has to stand somewhere, and a session with no tree stands here. The tree when
    // there is one; otherwise where the image expects a process to be, and `/` when it said
    // nothing or named a directory it never created.
    match share()? {
        Some(root) => set_cwd(&root)?,
        None => match image.working_dir.as_deref().map(Path::new) {
            Some(stated) if set_cwd(stated).is_ok() => {}
            _ => set_cwd(Path::new("/"))?,
        },
    }

    Ok((open_port()?, image))
}

/// What the base image expects, as the host left it in the boot root.
///
/// An absent file is the default rather than a failure. The only way to have one is to drive
/// the boot role by hand — a console server writes it on every boot, and the guest binary is
/// embedded in that same server, so the two cannot be different builds — and the default is
/// exactly the behaviour this end had before there was a spec to read.
///
/// A file that *is* there and does not decode is the other case, and that one is reported: it
/// means the two `contract` modules have drifted, which nothing else would catch.
fn image_spec() -> anyhow::Result<ImageSpec> {
    let encoded = match std::fs::read(IMAGE_SPEC_PATH) {
        Ok(encoded) => encoded,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ImageSpec::default()),
        Err(e) => anyhow::bail!("reading {IMAGE_SPEC_PATH}: {e}"),
    };

    bson::deserialize_from_slice(&encoded)
        .map_err(|e| anyhow::anyhow!("decoding {IMAGE_SPEC_PATH}: {e}"))
}

/// `mount(2)`, creating the target first.
///
/// Errors come back as [`io::Error`] rather than being raised, because half of the mounts
/// here are best-effort: a pseudo-filesystem that is already up is a failure worth
/// ignoring, and only the caller knows which kind it is asking for.
fn mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong) -> io::Result<()> {
    let _ = std::fs::create_dir_all(target);
    let (source, target, fstype) = (cstr(source), cstr(target), cstr(fstype));
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            flags,
            ptr::null(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The pseudo-filesystems every other step assumes.
///
/// Best-effort throughout: this runs twice — once on the boot root and once on the
/// overlay — and the second time some of them may already be where they need to be. A
/// failure that matters shows up as the next step not finding what it needs, with a
/// message about that instead.
fn mount_pseudo() {
    let _ = mount("proc", "/proc", "proc", 0);
    let _ = mount("devtmpfs", "/dev", "devtmpfs", 0);
    let _ = mount("sysfs", "/sys", "sysfs", 0);
}

/// Overlay the writable session image over the read-only base and pivot into it.
///
/// After this returns, `/` is the overlay: reads fall through to the base image, writes
/// land on the upper and stay there for the life of the session.
fn mount_root(lower_dev: &str, upper_dev: &str) -> anyhow::Result<()> {
    // `pivot_root` refuses with EINVAL while the root mount has shared propagation, so
    // the whole tree goes private first.
    let rc = unsafe {
        libc::mount(
            ptr::null(),
            cstr("/").as_ptr(),
            ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            ptr::null(),
        )
    };
    if rc != 0 {
        anyhow::bail!("making / private: {}", io::Error::last_os_error());
    }

    // A tmpfs to assemble in, so none of the plumbing directories has to exist on the
    // read-only base image.
    mount("tmpfs", "/mnt", "tmpfs", 0).map_err(|e| anyhow::anyhow!("tmpfs on /mnt: {e}"))?;
    for dir in ["/mnt/lower", "/mnt/upper", "/mnt/newroot"] {
        let _ = std::fs::create_dir_all(dir);
    }

    // `MS_RDONLY` is not a preference: the disk is attached read-only, and the kernel
    // refuses the mount with EACCES without it.
    mount(lower_dev, "/mnt/lower", "erofs", libc::MS_RDONLY)
        .map_err(|e| anyhow::anyhow!("mounting {lower_dev} as the base image: {e}"))?;
    mount(upper_dev, "/mnt/upper", "ext4", 0)
        .map_err(|e| anyhow::anyhow!("mounting {upper_dev} as the session image: {e}"))?;
    let _ = std::fs::create_dir_all("/mnt/upper/upper");
    let _ = std::fs::create_dir_all("/mnt/upper/work");

    let options = cstr("lowerdir=/mnt/lower,upperdir=/mnt/upper/upper,workdir=/mnt/upper/work");
    let rc = unsafe {
        libc::mount(
            cstr("overlay").as_ptr(),
            cstr("/mnt/newroot").as_ptr(),
            cstr("overlay").as_ptr(),
            0,
            options.as_ptr() as *const libc::c_void,
        )
    };
    if rc != 0 {
        anyhow::bail!(
            "overlaying the session over the base image: {}",
            io::Error::last_os_error()
        );
    }

    pivot("/mnt/newroot")
}

/// Make `new_root` the root and detach what was there.
fn pivot(new_root: &str) -> anyhow::Result<()> {
    let _ = std::fs::create_dir_all(format!("{new_root}/oldroot"));

    // `pivot_root(".", "oldroot")` with the cwd at the new root — the spelling that does
    // not depend on the old root still being namable afterwards.
    if unsafe { libc::chdir(cstr(new_root).as_ptr()) } != 0 {
        anyhow::bail!("entering {new_root}: {}", io::Error::last_os_error());
    }
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pivot_root,
            cstr(".").as_ptr(),
            cstr("oldroot").as_ptr(),
        )
    };
    if rc != 0 {
        anyhow::bail!("pivoting onto {new_root}: {}", io::Error::last_os_error());
    }
    if unsafe { libc::chroot(cstr(".").as_ptr()) } != 0 {
        anyhow::bail!("chroot after the pivot: {}", io::Error::last_os_error());
    }
    if unsafe { libc::chdir(cstr("/").as_ptr()) } != 0 {
        anyhow::bail!("returning to /: {}", io::Error::last_os_error());
    }

    // Lazily, because the overlay holds its lower, upper and work directories through
    // handles of its own — they keep working after the names they were mounted under
    // leave the namespace.
    if unsafe { libc::umount2(cstr("/oldroot").as_ptr(), libc::MNT_DETACH) } != 0 {
        anyhow::bail!("detaching the boot root: {}", io::Error::last_os_error());
    }
    let _ = std::fs::remove_dir("/oldroot");
    Ok(())
}

/// Mount the workspace share, if the boot named one, and say where it landed.
///
/// One virtio-fs device for the whole workspace: what is underneath it — how many
/// volumes, of what kind, mounted where — is cortex's business on the host side, and
/// arrives here already assembled into one tree.
fn share() -> anyhow::Result<Option<PathBuf>> {
    let Ok(spec) = std::env::var(SHARE_ENV) else {
        return Ok(None);
    };
    // Split on the first `:` only: a guest path may not contain one, but the split has
    // to be unambiguous rather than merely usually right.
    let Some((tag, mountpoint)) = spec.split_once(':') else {
        anyhow::bail!("{SHARE_ENV} is `{spec}`, which is not `tag:/mountpoint`");
    };

    mount(tag, mountpoint, "virtiofs", 0)
        .map_err(|e| anyhow::anyhow!("mounting the workspace at {mountpoint}: {e}"))?;
    Ok(Some(PathBuf::from(mountpoint)))
}

fn set_cwd(dir: &Path) -> anyhow::Result<()> {
    std::env::set_current_dir(dir).map_err(|e| anyhow::anyhow!("entering {}: {e}", dir.display()))
}

/// Open the virtio-console port the console session runs over.
///
/// Two ways to find it, because only one of them needs a udev the guest may not have:
/// the `/dev/virtio-ports/<name>` symlink is what a full system's rules create, and the
/// `name` attribute under `/sys/class/virtio-ports` is what those rules read. Falling
/// back to the second is what makes a minimal base image work.
///
/// Read-write, and opened exactly once. The virtio-console driver allows a single opener
/// per port and answers a second with `EBUSY`, which is why the two directions the agent
/// needs come from `dup`ing this handle rather than from opening the device again.
fn open_port() -> anyhow::Result<File> {
    let deadline = std::time::Instant::now() + PORT_TIMEOUT;
    loop {
        if let Some(path) = find_port() {
            return File::options()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| anyhow::anyhow!("opening the console port {}: {e}", path.display()));
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "no virtio-console port named `{PORT_NAME}` appeared — the boot attached none, \
                 or the guest kernel has no virtio-console driver"
            );
        }
        std::thread::sleep(PORT_POLL);
    }
}

/// The device node for [`PORT_NAME`], if it is there yet.
fn find_port() -> Option<PathBuf> {
    let by_name = PathBuf::from("/dev/virtio-ports").join(PORT_NAME);
    if by_name.exists() {
        return Some(by_name);
    }

    // Each entry is a port; its `name` attribute is what the host tagged it with, and the
    // entry's own name (`vport0p1`) is the device node under `/dev`.
    for entry in std::fs::read_dir("/sys/class/virtio-ports").ok()?.flatten() {
        let named = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if named.trim() != PORT_NAME {
            continue;
        }
        let node = PathBuf::from("/dev").join(entry.file_name());
        if node.exists() {
            return Some(node);
        }
    }
    None
}

/// Every path and option below crosses into C, and none of them can hold an interior nul:
/// they are literals here or paths the host wrote into the environment.
fn cstr(s: &str) -> CString {
    CString::new(s).expect("no interior nul in a mount argument")
}
