//! S3-over-virtiofs spike: boot a microVM with an [`S3Volume`] attached as a
//! virtio-fs share and, inside the guest, `ls` + read a real S3 object — proving
//! the workspace VFS can be served into an msb_krun guest with no daemon, no
//! host mount, and no TCP tunnel.
//!
//! S3 credentials come from the environment (source `agent-k/.env` first):
//!
//! ```text
//! set -a; . ../agent-k/.env; set +a
//! cargo run --features krun,s3 --bin apply_krun_s3
//! ```
//!
//! Needs `AWS_S3_BUCKET`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`
//! (`AWS_DEFAULT_REGION` defaults to us-east-1; `AWS_S3_ENDPOINT` /
//! `AWS_S3_KEY_PREFIX` optional). Rootfs + kernel are provisioned like
//! `apply_krun`.

use std::path::{Path, PathBuf};
use std::process::Command;

use cortex::{Dirent, Mountable, PosixAdapter, S3Config, S3Volume};
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
        eprintln!("extracting {} -> {}", tarball.display(), rootfs.display());
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

fn s3_config_from_env() -> S3Config {
    let var = |k: &str| {
        std::env::var(k).unwrap_or_else(|_| fail(format!("missing env {k} (source agent-k/.env)")))
    };
    S3Config {
        bucket: var("AWS_S3_BUCKET"),
        region: std::env::var("AWS_DEFAULT_REGION").unwrap_or_else(|_| "us-east-1".into()),
        access_key_id: var("AWS_ACCESS_KEY_ID"),
        secret_access_key: var("AWS_SECRET_ACCESS_KEY"),
        endpoint: std::env::var("AWS_S3_ENDPOINT").ok(),
        key_prefix: std::env::var("AWS_S3_KEY_PREFIX").ok(),
    }
}

/// Bounded BFS for the smallest file under the bucket, so the guest `cat` prints
/// something sane. Returns the object's key (relative to the mount).
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
    let cfg = s3_config_from_env();
    eprintln!("S3 bucket: {} (region {})", cfg.bucket, cfg.region);

    // Build the volume once host-side to discover a target object, then hand a
    // fresh volume to the guest adapter.
    let probe = S3Volume::new(&cfg).unwrap_or_else(|e| fail(format!("S3 connect: {e}")));
    // A fixed key (S3_BENCH_KEY) makes this comparable to agent-k's tunnel
    // benchmark on the same object; otherwise pick the smallest file to read.
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

    let s3 = S3Volume::new(&cfg).unwrap_or_else(|e| fail(format!("S3 connect: {e}")));
    // Read benchmark: cold (drop_caches) sequential reads of the same object,
    // 3×, reporting busybox dd's own byte/rate summary. Comparable to agent-k's
    // tunnel `s3_read_throughput` when both target the same S3_BENCH_KEY.
    // Single line, no `$()`/nested quotes/parens (msb_krun's exec-arg transport
    // chokes on newlines and mangled the command substitution). The key has no
    // spaces, so it needs no quoting.
    let script = format!(
        "mount -t virtiofs s3 /mnt; \
         for i in 1 2 3; do sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null; \
         echo read_$i; dd if=/mnt/{key} of=/dev/null bs=1M 2>&1 | tail -1; done"
    );

    let result = VmBuilder::new()
        .machine(|m| m.vcpus(1).memory_mib(512))
        .kernel(|k| k.krunfw_path(&kernel))
        .fs(|fs| {
            fs.root(&rootfs)
                .tag("s3")
                .custom(Box::new(PosixAdapter::new(s3)))
        })
        .exec(|e| e.path("/bin/sh").args(["-c", script.as_str()]))
        .build()
        .and_then(|vm| vm.enter());

    match result {
        Ok(never) => match never {},
        Err(e) => fail(format!("boot failed before entering the VM: {e}")),
    }
}
