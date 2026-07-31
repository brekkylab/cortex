//! Boot a microVM with a cortex [`Workspace`] attached as a FUSE filesystem
//! and, inside the guest, read the file it serves.
//!
//! This is a binary rather than a test on purpose: on guest shutdown libkrun
//! calls `_exit()`, which tears down the whole process. That is exactly what
//! you want from a standalone command — run it, watch the guest print, done —
//! but it makes an in-process test assertion impossible.
//!
//! On first run it downloads an Alpine minirootfs into `<crate>/data/rootfs`
//! (git-ignored) and auto-detects the libkrunfw kernel. Both can be overridden:
//!
//! ```text
//! apply_krun                        # auto: download rootfs + detect kernel
//! CORTEX_TEST_ROOTFS=/some/rootfs apply_krun
//! CORTEX_TEST_KERNEL=/some/libkrunfw.dylib apply_krun
//! ```
//!
//! On success the guest runs `mount -t virtiofs cortex /mnt && cat /mnt/hello.txt`,
//! so you should see `Hello from cortex!` on stdout before the process exits
//! with the guest's exit code.

use std::path::{Path, PathBuf};
use std::process::Command;

use cortex::{FileExt, InMemVolume, Mountable, PosixAdapter, Workspace};
use msb_krun::VmBuilder;

/// What the guest reads out of `/mnt/hello.txt`.
const FILE_CONTENT: &[u8] = b"Hello from cortex!\n";

/// Guest is aarch64 (libkrun on Apple Silicon), so the rootfs must match.
const ALPINE_URL: &str = "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/aarch64/alpine-minirootfs-3.24.1-aarch64.tar.gz";
const ALPINE_SHA256: &str = "f55a90f69052c5bd6f92cb09a8f47065970830b194c917a006fb94028e721259";
const ALPINE_TARBALL: &str = "alpine-minirootfs-3.24.1-aarch64.tar.gz";

/// `<crate>/data`, resolved at build time so it works regardless of cwd.
fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data")
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

/// Run a command, exiting the process if it can't be spawned or exits non-zero.
fn run(cmd: &mut Command) {
    let display = format!("{cmd:?}");
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => fail(format!("command failed ({s}): {display}")),
        Err(e) => fail(format!("could not run {display}: {e}")),
    }
}

/// Download + extract the Alpine minirootfs into `<data>/rootfs` if not already
/// present, then make sure `/mnt` exists for the guest to mount onto.
fn ensure_rootfs() -> PathBuf {
    let data = data_dir();
    let rootfs = data.join("rootfs");

    // Sentinel: a regular file that ships in the minirootfs. (Don't probe
    // `bin/sh` — it's a symlink to the absolute guest path `/bin/busybox`, which
    // never resolves on the host, so `exists()` would always be false.)
    if !rootfs.join("etc/alpine-release").exists() {
        std::fs::create_dir_all(&rootfs)
            .unwrap_or_else(|e| fail(format!("create {}: {e}", rootfs.display())));

        let tarball = data.join(ALPINE_TARBALL);
        if !tarball.exists() {
            eprintln!("downloading {ALPINE_URL}");
            run(Command::new("curl")
                .arg("-fsSL")
                .arg(ALPINE_URL)
                .arg("-o")
                .arg(&tarball));
        }

        verify_sha256(&tarball, ALPINE_SHA256);

        eprintln!("extracting {} -> {}", tarball.display(), rootfs.display());
        run(Command::new("tar")
            .arg("-xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(&rootfs));
    }

    // The guest mounts the toy fs at /mnt; the minirootfs may not ship the dir.
    std::fs::create_dir_all(rootfs.join("mnt"))
        .unwrap_or_else(|e| fail(format!("create /mnt in rootfs: {e}")));

    rootfs
}

/// Verify a file's SHA-256 via `shasum`, exiting on mismatch.
fn verify_sha256(file: &Path, expected: &str) {
    let out = Command::new("shasum")
        .arg("-a")
        .arg("256")
        .arg(file)
        .output()
        .unwrap_or_else(|e| fail(format!("run shasum: {e}")));
    if !out.status.success() {
        fail(format!("shasum failed on {}", file.display()));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let got = stdout.split_whitespace().next().unwrap_or("");
    if got != expected {
        fail(format!(
            "sha256 mismatch for {}:\n  expected {expected}\n  got      {got}\n\
             delete the file and retry",
            file.display()
        ));
    }
    eprintln!("sha256 ok: {}", file.display());
}

/// Locate the libkrunfw kernel: explicit override first, then the usual install
/// locations on macOS.
fn resolve_kernel() -> PathBuf {
    if let Ok(k) = std::env::var("CORTEX_TEST_KERNEL") {
        return PathBuf::from(k);
    }
    let mut candidates = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home).join(".microsandbox/lib/libkrunfw.dylib"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/lib/libkrunfw.dylib"));
    candidates.push(PathBuf::from("/usr/local/lib/libkrunfw.dylib"));

    candidates
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            fail(
                "could not find libkrunfw.dylib; set CORTEX_TEST_KERNEL to its path \
                 (install microsandbox or `brew install libkrunfw`)",
            )
        })
}

/// A [`Workspace`] served into the guest: a single [`InMemVolume`] mounted at
/// the workspace root, holding one file at `./hello.txt`.
///
/// Mounting at the empty (root) path gives the workspace a directory to serve
/// as inode 1, so the guest's `mount` (which `getattr`s the root) and `cat`
/// both resolve; the file itself lives at `hello.txt` under that mount.
fn build_workspace() -> Workspace {
    let vol = InMemVolume::new();
    let file = vol
        .create(Path::new("hello.txt"))
        .expect("fresh volume: root exists and hello.txt is free");
    file.write_all_at(FILE_CONTENT, 0)
        .expect("in-memory positioned write is infallible");

    Workspace::new()
        .try_with_mount("", vol)
        .expect("the empty mount path never escapes the workspace root")
}

/// Boot a microVM with a cortex [`Workspace`] served as a virtio-fs share,
/// tagged `cortex`. Inside the guest:
///
/// ```text
/// mount -t virtiofs cortex /mnt && cat /mnt/hello.txt
/// ```
///
/// [`Workspace`] is a path-addressed [`Mountable`](cortex::Mountable) backend;
/// [`PosixAdapter`] wraps it into the FUSE-shaped
/// `Box<dyn DynFileSystem + Send + Sync>` that `FsBuilder::custom` attaches to
/// the guest — so the workspace is served straight from this process, no daemon,
/// no host mount.
///
/// A real boot also needs a populated `rootfs` and a matching libkrunfw
/// `kernel` firmware, both of which this binary provisions.
fn boot_with_workspace(
    rootfs: impl AsRef<Path>,
    kernel: impl AsRef<Path>,
) -> msb_krun::Result<std::convert::Infallible> {
    VmBuilder::new()
        .machine(|m| m.vcpus(1).memory_mib(512))
        .kernel(|k| k.krunfw_path(kernel.as_ref()))
        .fs(|fs| {
            // Mount 0: the guest root filesystem (host passthrough).
            // Mount 1: our workspace, reachable inside the guest at virtio-fs tag `cortex`.
            fs.root(rootfs.as_ref())
                .tag("cortex")
                .custom(Box::new(PosixAdapter::new(build_workspace())))
        })
        .exec(|e| {
            e.path("/bin/sh")
                .args(["-c", "mount -t virtiofs cortex /mnt && cat /mnt/hello.txt"])
        })
        .build()?
        .enter()
}

fn main() {
    let rootfs = match std::env::var("CORTEX_TEST_ROOTFS") {
        Ok(r) => PathBuf::from(r),
        Err(_) => ensure_rootfs(),
    };
    let kernel = resolve_kernel();

    eprintln!(
        "booting microVM with cortex workspace:\n  rootfs = {}\n  kernel = {}",
        rootfs.display(),
        kernel.display(),
    );

    // On success `boot_with_workspace` never returns: the guest runs, prints the
    // file, and libkrun `_exit()`s this process with the guest's exit code. So
    // the only way control reaches here is a pre-launch error. The `Ok` variant
    // holds `Infallible`, matched with the empty `match` below.
    match boot_with_workspace(rootfs, kernel) {
        Ok(never) => match never {},
        Err(e) => fail(format!("boot failed before entering the VM: {e}")),
    }
}
