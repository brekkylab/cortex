//! Boot a microVM whose workspace is a cortex [`S3Volume`], attached through
//! **microsandbox's pluggable fs-backend registry** rather than by constructing
//! the backend inline (that inline path is `apply_krun_s3`).
//!
//! This exercises the exact seam an integrator uses:
//!   1. Register a factory by name (`register_fs_backend`) — the factory turns a
//!      serialized [`FsBackendSpec`] into a live `DynFileSystem`.
//!   2. Hand the runtime an `FsBackendSpec` (this is what an SDK would serialize
//!      into the launch config alongside `mounts`/`env`).
//!   3. The runtime resolves the spec through the registry (`build_fs_backend`)
//!      and attaches the result as a virtio-fs device — the same three lines
//!      `microsandbox-runtime`'s `build_vm` runs for every `fs_backends` entry.
//!
//! S3 credentials come from the environment (source `agent-k/.env` first):
//!
//! ```text
//! set -a; . ../agent-k/.env; set +a
//! cargo run --features msb --bin apply_msb_s3
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use cortex::msb::{S3Params, S3_BACKEND_TYPE, register_s3_backend};
use cortex::{Dirent, Mountable, S3Config, S3Volume};
use microsandbox_runtime::fs_backend::{self, FsBackendSpec};
use msb_krun::VmBuilder;

const ALPINE_URL: &str = "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/aarch64/alpine-minirootfs-3.24.1-aarch64.tar.gz";
const ALPINE_SHA256: &str = "f55a90f69052c5bd6f92cb09a8f47065970830b194c917a006fb94028e721259";
const ALPINE_TARBALL: &str = "alpine-minirootfs-3.24.1-aarch64.tar.gz";

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data")
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

fn run(cmd: &mut Command) {
    let display = format!("{cmd:?}");
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => fail(format!("command failed ({s}): {display}")),
        Err(e) => fail(format!("could not run {display}: {e}")),
    }
}

fn ensure_rootfs() -> PathBuf {
    let data = data_dir();
    let rootfs = data.join("rootfs");
    if !rootfs.join("etc/alpine-release").exists() {
        std::fs::create_dir_all(&rootfs)
            .unwrap_or_else(|e| fail(format!("create {}: {e}", rootfs.display())));
        let tarball = data.join(ALPINE_TARBALL);
        if !tarball.exists() {
            eprintln!("downloading {ALPINE_URL}");
            run(Command::new("curl").arg("-fsSL").arg(ALPINE_URL).arg("-o").arg(&tarball));
        }
        verify_sha256(&tarball, ALPINE_SHA256);
        run(Command::new("tar").arg("-xzf").arg(&tarball).arg("-C").arg(&rootfs));
    }
    std::fs::create_dir_all(rootfs.join("mnt"))
        .unwrap_or_else(|e| fail(format!("create /mnt in rootfs: {e}")));
    rootfs
}

fn verify_sha256(file: &Path, expected: &str) {
    let out = Command::new("shasum")
        .arg("-a")
        .arg("256")
        .arg(file)
        .output()
        .unwrap_or_else(|e| fail(format!("run shasum: {e}")));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let got = stdout.split_whitespace().next().unwrap_or("");
    if got != expected {
        fail(format!("sha256 mismatch for {}", file.display()));
    }
}

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
        .unwrap_or_else(|| fail("could not find libkrunfw.dylib; set CORTEX_TEST_KERNEL"))
}

/// Bounded BFS for the smallest file under the bucket, so the guest `cat`
/// prints something sane. Returns the object's key.
fn find_smallest_file(vol: &S3Volume) -> Option<(String, u64)> {
    let mut queue = vec![String::new()];
    let mut best: Option<(String, u64)> = None;
    let mut budget = 40u32;
    while let Some(dir) = queue.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let Ok(entries) = vol.list(Path::new(&dir)) else {
            continue;
        };
        for e in entries {
            let p = if dir.is_empty() {
                e.name().to_string()
            } else {
                format!("{dir}/{}", e.name())
            };
            match e {
                Dirent::Dir(_) => queue.push(p),
                Dirent::File(_) => {
                    let size = vol.stat(Path::new(&p)).map(|s| s.size).unwrap_or(u64::MAX);
                    if best.as_ref().is_none_or(|(_, b)| size < *b) {
                        best = Some((p, size));
                    }
                }
            }
        }
    }
    best
}

fn main() {
    let params = S3Params::from_env().unwrap_or_else(|e| fail(format!("{e} (source agent-k/.env)")));
    eprintln!("S3 bucket: {} (region {})", params.bucket, params.region);

    // (1) Register the factory (shared with the custom `msb` binary via
    //     `cortex::msb`). In a real integrator this runs in the sandbox
    //     process's `main` before boot; the runtime then resolves any
    //     `FsBackendSpec` of this type through it.
    register_s3_backend();

    // Probe host-side to pick an object to read.
    let probe =
        S3Volume::new(&S3Config::from(&params)).unwrap_or_else(|e| fail(format!("S3 connect: {e}")));
    let (key, size) = match std::env::var("S3_BENCH_KEY") {
        Ok(k) => {
            let sz = probe
                .stat(Path::new(&k))
                .map(|s| s.size)
                .unwrap_or_else(|e| fail(format!("stat {k}: {e}")));
            (k, sz)
        }
        Err(_) => find_smallest_file(&probe)
            .unwrap_or_else(|| fail("no object found under the bucket to read")),
    };
    eprintln!("target object: {key} ({size} bytes)");

    // (2) The spec an SDK would serialize into the launch config.
    let spec = FsBackendSpec {
        tag: "s3".to_string(),
        backend_type: S3_BACKEND_TYPE.to_string(),
        params: serde_json::to_string(&params).expect("serialize params"),
    };

    // (3) Resolve the spec through the registry — the same call
    //     microsandbox-runtime's `build_vm` makes for each `fs_backends` entry.
    let backend = fs_backend::build_fs_backend(&spec)
        .unwrap_or_else(|e| fail(format!("resolve fs-backend {S3_BACKEND_TYPE:?}: {e}")));
    eprintln!("resolved backend {S3_BACKEND_TYPE:?} via microsandbox fs-backend registry");

    let rootfs = match std::env::var("CORTEX_TEST_ROOTFS") {
        Ok(r) => PathBuf::from(r),
        Err(_) => ensure_rootfs(),
    };
    let kernel = resolve_kernel();
    eprintln!(
        "booting microVM:\n  rootfs = {}\n  kernel = {}",
        rootfs.display(),
        kernel.display()
    );

    // Single line: mount the resolved backend and read the object in the guest.
    let script = format!(
        "mount -t virtiofs s3 /mnt; \
         for i in 1 2 3; do sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null; \
         echo read_$i; dd if=/mnt/{key} of=/dev/null bs=1M 2>&1 | tail -1; done"
    );

    // (4) Boot with the registry-resolved backend attached as virtio-fs.
    let result = VmBuilder::new()
        .machine(|m| m.vcpus(1).memory_mib(512))
        .kernel(|k| k.krunfw_path(&kernel))
        .fs(|fs| fs.root(&rootfs).tag("s3").custom(backend))
        .exec(|e| e.path("/bin/sh").args(["-c", script.as_str()]))
        .build()
        .and_then(|vm| vm.enter());

    match result {
        Ok(never) => match never {},
        Err(e) => fail(format!("boot failed before entering the VM: {e}")),
    }
}
