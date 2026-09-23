//! The four things a boot needs on disk, and where each one comes from.
//!
//! | | lifetime | cost |
//! |---|---|---|
//! | the libkrunfw kernel | cached, shared | downloaded once, pinned by digest |
//! | the base image | cached, shared | pulled or built once per image |
//! | the session image (ext4) | one session | formatted per boot, sparse |
//! | the boot root | one session | a directory with two files in it |
//!
//! The split down the middle of that table is the point. Two of them are shared and
//! expensive and live under [`home`]; two are this session's, live in a temp directory, and
//! are deleted when the value holding them drops. Nothing a session writes can reach anything
//! another session reads — which is the whole reason the guest boots onto an overlay instead
//! of onto a writable directory.
//!
//! # Two ways to a base image
//!
//! An **OCI reference** is pulled: layers arrive from a registry, each is
//! encoded as its own EROFS, and a descriptor stitches the set into one disk. That is the
//! path for `python:3.13` and anything else somebody already publishes, and the layer store
//! underneath it is content-addressed, so images sharing a base share the copy of it.
//!
//! A **rootfs tarball** is the default when no reference is given: `ingest_compressed_tar`
//! reads it into a file tree and `write_erofs` encodes that tree as a read-only image. No
//! `mkfs`, no privileges, no loop device, and no registry to be reachable — which is what
//! keeps a boot working the same way on a laptop, in CI and offline.
//!
//! Either way what comes out is a disk the guest mounts as `erofs`, and — for an OCI image —
//! an [`ImageSpec`] saying what the image expects of a process running in it. The tarball has
//! nothing to say, and says nothing.
//!

use std::{
    io,
    path::{Path, PathBuf},
};

use microsandbox_image::{
    GlobalCache, Platform, PullOptions, Reference, Registry, RootfsMaterialization,
    ext4::{Ext4FormatOptions, format_ext4},
    tar::{Compression, ingest_compressed_tar},
    tree::ResourceLimits,
};

use crate::contract::{GUEST_BIN_PATH, IMAGE_SPEC_PATH, ImageSpec};

/// The guest half, cross-compiled and embedded by `build.rs`. Written into every boot
/// root, which is why the guest crate optimises for size.
const GUEST_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cortex-uvm-guest"));

/// How large the session's ext4 image claims to be.
///
/// Sparse, so this is a ceiling and not an allocation: a session that writes a kilobyte
/// occupies a kilobyte plus the filesystem's own metadata. What the number bounds is how
/// much a runaway command can write before the guest reports a full disk, and two
/// gibibytes is enough for a package install and small enough that a stray `dd` does not
/// take the host's disk with it.
const SESSION_IMAGE_BYTES: u64 = 2 << 30;

/// 16 MiB of journal. The default is four times that, which is most of a small session's
/// image spent on a log for writes that are about to be thrown away.
const SESSION_JOURNAL_BLOCKS: u32 = 4096;

/// How long a provisioning step has to take before finishing it is worth a line. Under this it
/// was cached, and nobody waited.
///
/// Unlike every other duration in this workspace, nothing branches on it: a step that runs past
/// this is not retried, refused or invalidated, it is only mentioned. See [`took`].
const ANNOUNCE_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

/// Base directory for what is shared between sessions — `CORTEX_UVM_HOME`, else
/// `$CORTEX_HOME/uvm`, else `$HOME/.cortex/uvm`.
///
/// `CORTEX_HOME` is the root everything cortex writes lives under, the server a client
/// writes out included; this server's own share of it is `uvm`.
pub fn home() -> io::Result<PathBuf> {
    std::env::var_os("CORTEX_UVM_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CORTEX_HOME").map(|h| PathBuf::from(h).join("uvm")))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cortex/uvm")))
        .ok_or_else(|| io::Error::other("none of CORTEX_UVM_HOME, CORTEX_HOME or HOME is set"))
}

/// Where microsandbox publishes its releases, and so the libkrunfw this server boots.
const KRUNFW_BASE_URL: &str = "https://github.com/superradcompany/microsandbox/releases/download";

/// Stands in for [`KRUNFW_BASE_URL`] — a mirror, or a server a test runs.
pub const KRUNFW_BASE_URL_ENV: &str = "CORTEX_UVM_KRUNFW_BASE_URL";

/// The microsandbox release the kernel is taken from.
///
/// **Moves with the `microsandbox-*` pins in the workspace manifest**, and a test says so:
/// `msb_krun` is what loads this library, and a libkrunfw from another release is one it was
/// never built against. Every release republishes it — same size, different digest — so a
/// release is named by its tag and a file by its hash, and neither is taken on trust.
pub const KRUNFW_TAG: &str = "v0.6.12";

/// One platform's libkrunfw in [`KRUNFW_TAG`]: the asset, its SHA-256 from the release's
/// `checksums.sha256`, and the name it is kept under — the versioned one `msb_krun` looks for.
struct Krunfw {
    asset: &'static str,
    sha256: &'static str,
    file: &'static str,
}

/// This host's libkrunfw, if the release has one. There is no Intel macOS build.
fn pinned_krunfw() -> Option<Krunfw> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some(Krunfw {
            asset: "libkrunfw-darwin-aarch64.dylib",
            sha256: "13db04fa42d8753a2ebc021bdf42ff4c65342bb5e5373ecde9da4273c9082f97",
            file: "libkrunfw.5.dylib",
        }),
        ("linux", "aarch64") => Some(Krunfw {
            asset: "libkrunfw-linux-aarch64.so",
            sha256: "88160091068302e0e0ffc02cb84faf9ca6d1dc6995cdcc6716f1bd92ecc0497f",
            file: "libkrunfw.so.5",
        }),
        ("linux", "x86_64") => Some(Krunfw {
            asset: "libkrunfw-linux-x86_64.so",
            sha256: "d395efaa21984cc6934c900519909a12c8148d9688cfc88f9da3b42132ae32c2",
            file: "libkrunfw.so.5",
        }),
        _ => None,
    }
}

/// Find the libkrunfw kernel, fetching it if nothing names one.
///
/// In order:
///
/// 1. `CORTEX_UVM_KERNEL` — what a caller named.
/// 2. The pinned release's, under [`home`]`/lib/<tag>/` — downloaded the first time and
///    checked against its digest every time.
/// 3. Wherever a package manager put one — **only when the download failed**, so that a host
///    with no network and a `brew install libkrunfw` still boots. Not first: a libkrunfw
///    installed for something else is not one this server's `msb_krun` was built against.
///
/// A library `dlopen`ed into a VMM is code that runs as the VM, which is why a download is
/// refused unless it hashes to the pin rather than trusted because of where it came from. On
/// macOS it needs no signature of ours: the boot process carries
/// `disable-library-validation`, and a file this process downloads itself is never
/// quarantined.
pub async fn resolve_kernel() -> anyhow::Result<PathBuf> {
    if let Some(kernel) = std::env::var_os("CORTEX_UVM_KERNEL") {
        return Ok(PathBuf::from(kernel));
    }

    let fetched = match pinned_krunfw() {
        Some(pin) => {
            let base =
                std::env::var(KRUNFW_BASE_URL_ENV).unwrap_or_else(|_| KRUNFW_BASE_URL.to_string());
            let url = format!("{base}/{KRUNFW_TAG}/{}", pin.asset);
            let dest = home()?.join("lib").join(KRUNFW_TAG).join(pin.file);
            match fetch_pinned(
                &url,
                pin.sha256,
                &dest,
                &format_args!("libkrunfw {KRUNFW_TAG}"),
            )
            .await
            {
                Ok(kernel) => return Ok(kernel),
                Err(e) => Some(e),
            }
        }
        None => None,
    };

    if let Some(kernel) = installed_kernel() {
        if let Some(e) = &fetched {
            eprintln!(
                "cortex-uvm-console: {e:#} — booting the libkrunfw at {} instead",
                kernel.display()
            );
        }
        return Ok(kernel);
    }

    Err(match fetched {
        Some(e) => e.context(
            "no libkrunfw: the pinned one could not be fetched and none is installed. Set \
             CORTEX_UVM_KERNEL to one, or install it (`brew install libkrunfw`)",
        ),
        None => anyhow::anyhow!(
            "no libkrunfw is published for {}-{}, and none is installed. Set \
             CORTEX_UVM_KERNEL to one",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    })
}

/// A libkrunfw a package manager installed, if there is one.
///
/// Both spellings of the name are checked: the versioned one a release ships, and the plain one
/// a package manager symlinks.
fn installed_kernel() -> Option<PathBuf> {
    let (versioned, plain) = match std::env::consts::OS {
        "macos" => ("libkrunfw.5.dylib", "libkrunfw.dylib"),
        "windows" => ("libkrunfw.dll", "libkrunfw.dll"),
        _ => ("libkrunfw.so.5", "libkrunfw.so"),
    };

    let mut candidates = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            PathBuf::from(&home)
                .join(".microsandbox/lib")
                .join(versioned),
        );
        candidates.push(PathBuf::from(&home).join(".microsandbox/lib").join(plain));
    }
    for dir in ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"] {
        candidates.push(PathBuf::from(dir).join(versioned));
        candidates.push(PathBuf::from(dir).join(plain));
    }

    candidates.into_iter().find(|p| p.exists())
}

/// An OCI reference to use as the base — `python:3.13`, or `python@sha256:…`.
///
/// A tag is resolved once and then cached by digest, so a session started later gets the
/// image the first one did rather than whatever the tag has moved on to. Which also means a
/// caller who wants to *follow* a tag is asking for something this does not do; a digest
/// says what you get and is the spelling to prefer.
pub const IMAGE_ENV: &str = "CORTEX_UVM_IMAGE";

/// A writable session image to reuse instead of a fresh one, as a host path.
///
/// A caller's knob and not a computed value: read from this server's own environment, like
/// [`IMAGE_ENV`]. A session that survives the console, for whoever wants one.
pub const SESSION_IMAGE_ENV: &str = "CORTEX_UVM_SESSION_IMAGE";

/// Pull an OCI image and make a base out of it.
///
/// The layered materialization is the one that fits: it writes each layer as its own EROFS,
/// a metadata EROFS that references them, and a VMDK descriptor stitching the set into one
/// disk. The guest mounts that disk as `erofs` exactly as it mounts a tarball's image — the
/// layering is a host-side detail — so nothing on the far side of the hypervisor changes.
/// Layers are content-addressed and shared, which is what makes two images built on the same
/// base cost one copy of it.
///
/// Anonymous. Credentials for a private registry go to
/// [`RegistryBuilder::auth`](microsandbox_image::RegistryBuilder::auth), and what is missing
/// is not that call but a way for a caller to hand them over: a console session says nothing
/// about a registry, and an environment variable holding a password is not a channel worth
/// adding by default.
/// A pulled image, as the parts cortex needs from it.
///
/// Not a disk. `microsandbox-image` stitches one of its own, but a commit has to be able to
/// add a layer to whatever it started from and that stitch cannot be added to — the data maps
/// behind it exist only during the pull. So what comes back is the layers, and cortex builds
/// its own base out of them. See `base::seeded`.
pub struct Pulled {
    pub manifest_digest: microsandbox_image::Digest,
    /// Each pulled layer's EROFS on this host, bottom first.
    pub layers: Vec<PathBuf>,
    pub config: microsandbox_image::ImageConfig,
}

pub async fn pull(reference: &str) -> anyhow::Result<Pulled> {
    let reference: Reference = reference
        .parse()
        .map_err(|e| anyhow::anyhow!("{IMAGE_ENV}: {reference} is not an OCI reference: {e}"))?;

    // Its own directory rather than `images/`, which holds one file per pinned rootfs. This
    // is a content-addressed store of layers, manifests and stitched descriptors, and the
    // cache decides its own layout inside it.
    let cache = GlobalCache::new(&home()?.join("oci"))
        .map_err(|e| anyhow::anyhow!("opening the image cache: {e}"))?;

    let registry = Registry::builder(Platform::host_linux(), cache.clone())
        .build()
        .map_err(|e| anyhow::anyhow!("building a registry client: {e}"))?;

    // A pull with everything already cached touches no network, so this runs on every boot
    // rather than being guarded by a path test: what "cached" means is the cache's to decide,
    // and a layer that was half-written is a case only it can see.
    eprintln!("cortex-uvm-console: resolving {reference}");
    let started = std::time::Instant::now();
    let pulled = registry
        .pull(
            &reference,
            &PullOptions {
                materialization: RootfsMaterialization::Layered,
                ..PullOptions::default()
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("pulling {reference}: {e}"))?;
    took(&format_args!("{reference}"), started);

    let mut layers = Vec::with_capacity(pulled.layer_diff_ids.len());
    for diff_id in &pulled.layer_diff_ids {
        let path = cache.layer_erofs_path(diff_id);
        anyhow::ensure!(
            path.exists(),
            "pulling {reference} left no layer at {}",
            path.display()
        );
        layers.push(path);
    }

    Ok(Pulled {
        manifest_digest: pulled.manifest_digest,
        layers,
        config: pulled.config,
    })
}

/// A fresh, empty ext4 image for one session's writes.
///
/// Owns the file: dropping this deletes it, which is what makes a session ephemeral
/// without anything having to remember to clean up. `CORTEX_UVM_SESSION_IMAGE` points at a
/// file to reuse instead — a session that survives the console, for a caller who wants
/// one — and that file is left alone.
///
/// A named image belongs to one console at a time and nothing enforces that. Two guests
/// mounting the same ext4 read-write is filesystem corruption, not a race that resolves, so
/// a caller who names one is the one deciding no other console has it.
pub struct SessionImage {
    path: PathBuf,
    /// Whether the file is ours to delete. False for a caller's own image.
    owned: bool,
}

impl SessionImage {
    pub fn create() -> anyhow::Result<SessionImage> {
        if let Some(path) = std::env::var_os(SESSION_IMAGE_ENV) {
            let path = PathBuf::from(path);
            if !path.exists() {
                format(&path)?;
            }
            return Ok(SessionImage { path, owned: false });
        }

        let path = unique(std::env::temp_dir(), "session.ext4");
        format(&path)?;
        Ok(SessionImage { path, owned: true })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionImage {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn format(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    format_ext4(
        path,
        &Ext4FormatOptions {
            size_bytes: SESSION_IMAGE_BYTES,
            journal_blocks: SESSION_JOURNAL_BLOCKS,
        },
    )
    .map_err(|e| anyhow::anyhow!("formatting {}: {e}", path.display()))?;
    Ok(())
}

/// The directory libkrun serves as the guest's virtio-fs root, holding the guest binary and
/// what the base image said about running things in it.
///
/// It is a real root only for the moment between the kernel handing over and the guest
/// pivoting onto the overlay — long enough to exec one file and read one other. Which is why
/// it is a fresh directory per session rather than something shared: it is writable by the
/// guest for that moment, and two guests sharing one would be two guests writing the same
/// tree.
pub struct BootRoot {
    path: PathBuf,
}

impl BootRoot {
    pub fn create(spec: &ImageSpec) -> anyhow::Result<BootRoot> {
        use std::os::unix::fs::PermissionsExt;

        let path = unique(std::env::temp_dir(), "boot");
        std::fs::create_dir_all(&path)?;
        let root = BootRoot { path };

        let guest = root.path.join(GUEST_BIN_PATH.trim_start_matches('/'));
        std::fs::write(&guest, GUEST_BIN)?;
        std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o755))?;

        // Written even when it says nothing. A boot root has one shape either way, so the
        // guest reads a spec that states nothing rather than reasoning about a missing file.
        let encoded = bson::serialize_to_vec(spec)
            .map_err(|e| anyhow::anyhow!("encoding the image spec: {e}"))?;
        let spec_path = root.path.join(IMAGE_SPEC_PATH.trim_start_matches('/'));
        std::fs::write(spec_path, encoded)?;

        Ok(root)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BootRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Where a committable session's guest writes the layer a `commit` produces.
///
/// One session's, and owned outright: dropping this removes the directory and whatever the
/// guest left in it. A layer can be hundreds of megabytes, so leaving one behind would be a
/// real cost — and the sweep that catches a killed server's leavings finds this too, because
/// it is named the same way everything else here is.
pub struct CommitScratch {
    path: PathBuf,
}

impl CommitScratch {
    pub fn create() -> anyhow::Result<CommitScratch> {
        let path = unique(std::env::temp_dir(), "commit");
        std::fs::create_dir_all(&path)
            .map_err(|e| anyhow::anyhow!("making {}: {e}", path.display()))?;
        Ok(CommitScratch { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CommitScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Marks a path as a console server's and carries the owning pid, so a later run can tell
/// what was abandoned from what is in use. See [`sweep_abandoned`].
pub const PREFIX: &str = "cortex-uvm-";

/// A path nothing else in this process or any other will pick, of the shape [`sweep_abandoned`]
/// can read a pid back out of.
///
/// The pid keeps two consoles apart and the counter keeps two boots of one console apart —
/// a `stop` and the next `start` are two images, and the first is still being deleted while
/// the second is being formatted.
pub fn unique(dir: PathBuf, what: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{PREFIX}{}-{n}-{what}", std::process::id()))
}

/// Delete what a server that was killed outright left behind.
///
/// Every value in this module removes its own file when it drops, which covers a session
/// ending any way that runs a destructor — a `stop`, a `quit`, a process exiting on its own.
/// What it cannot cover is `SIGKILL`, and what is lost by it is a sparse image that may hold
/// everything a session wrote, plus the boot root under it.
///
/// So this sweep is the only thing that ever reclaims those, and it runs at the start of
/// every server rather than at the end of any: a process that was killed is not one that
/// gets to clean up, and the next one is the first thing that can.
///
/// Making that window disappear rather than sweeping it means never giving the file a name
/// the filesystem keeps: create it, unlink it, and hand the child the descriptor. That is
/// the fix to reach for before a spec on this backend is ever allowed to carry a real
/// secret — which it cannot today, since this crate pins `cortex` to `krun` and has no way
/// to forward `s3` or `notion`.
///
/// So the pid in the name is what a later run reads. `kill(pid, 0)` sends no signal and only
/// reports whether the pid can be signalled: `ESRCH` — no such process — is the one answer
/// that means the path is abandoned, where `EPERM` says the pid is alive under another user
/// and its files are not ours to remove.
///
/// Called once when a server starts. Best-effort throughout, because none of it is this
/// session's to succeed at: a path that cannot be removed is left for the next run, which is
/// where it was already.
pub fn sweep_abandoned() {
    // Both, because the two differ on macOS: a socket has to live under `/tmp` for its path
    // to fit in `sockaddr_un`, where everything else uses the per-user temp directory.
    let mut swept = vec![std::env::temp_dir()];
    if !swept.contains(&PathBuf::from("/tmp")) {
        swept.push(PathBuf::from("/tmp"));
    }

    for dir in swept {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|n| n.strip_prefix(PREFIX))
                .and_then(|rest| rest.split('-').next())
                .and_then(|pid| pid.parse::<i32>().ok())
            else {
                continue;
            };
            // SAFETY: `kill` with signal 0 performs only the permission/existence check; it
            // cannot affect this or any other process.
            let gone = unsafe { libc::kill(pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if !gone {
                continue;
            }
            let path = entry.path();
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
}

/// A base rootfs tarball, by URL and by the digest it has to hash to.
///
/// Pinned per architecture rather than resolved, because "latest" is a moving target and
/// the thing being downloaded decides what every command in every guest runs.
pub struct Rootfs {
    url: &'static str,
    /// Lowercase hex SHA-256 of the tarball.
    sha256: &'static str,
    /// Names the cached image, so a change to the pin above is a different cache entry
    /// rather than a stale one.
    name: &'static str,
}

impl Rootfs {
    pub fn host() -> Rootfs {
        match std::env::consts::ARCH {
            "aarch64" => Rootfs {
                url: "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/aarch64/alpine-minirootfs-3.24.1-aarch64.tar.gz",
                sha256: "f55a90f69052c5bd6f92cb09a8f47065970830b194c917a006fb94028e721259",
                name: "alpine-3.24.1-aarch64",
            },
            "x86_64" => Rootfs {
                url: "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/x86_64/alpine-minirootfs-3.24.1-x86_64.tar.gz",
                sha256: "41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081",
                name: "alpine-3.24.1-x86_64",
            },
            // A guest runs the host's architecture, so there is nothing to fall back to.
            other => panic!("no base rootfs pinned for a {other} host — name an image instead"),
        }
    }

    /// The tarball on disk, downloading and verifying it if it is not cached.
    pub async fn fetch(&self) -> anyhow::Result<PathBuf> {
        let dest = home()?.join("rootfs").join(format!("{}.tar.gz", self.name));
        fetch_pinned(self.url, self.sha256, &dest, &format_args!("{}", self.name)).await
    }
}

/// `dest`, if it hashes to `sha256` — else `url`, downloaded there and checked first.
///
/// Downloaded to a path of its own and renamed, so an interrupted download is never mistaken
/// for a cached one, and two sessions fetching at once do not write the same file. `curl`
/// rather than an HTTP client, because a TLS stack is a large thing to link for a handful of
/// GETs.
async fn fetch_pinned(
    url: &str,
    sha256: &str,
    dest: &Path,
    what: &std::fmt::Arguments<'_>,
) -> anyhow::Result<PathBuf> {
    if dest.exists() && digest(dest).await? == sha256 {
        return Ok(dest.to_path_buf());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp = unique(
        dest.parent().map(Path::to_path_buf).unwrap_or_default(),
        "download",
    );
    eprintln!("cortex-uvm-console: downloading {url}");
    let started = std::time::Instant::now();
    let status = tokio::process::Command::new("curl")
        .arg("-fsSL")
        .arg(url)
        .arg("-o")
        .arg(&tmp)
        .status()
        .await
        .map_err(|e| anyhow::anyhow!("running curl: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("downloading {url} failed ({status})");
    }

    let got = digest(&tmp).await?;
    if got != sha256 {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("{url} hashed to {got}, not the pinned {sha256}");
    }

    std::fs::rename(&tmp, dest)?;
    took(what, started);
    Ok(dest.to_path_buf())
}

/// Say that something slow is done, and how slow it was.
///
/// Only when it was actually slow. Provisioning is announced before it starts, because the point
/// of announcing is a wait somebody is sitting through; a cached boot does the same work in
/// milliseconds and a second line about it is noise on every session that ever starts.
///
/// **The pairing is what carries the information.** One line and no second one means it was
/// already there; one line and then another means the seconds in between were this.
fn took(what: &std::fmt::Arguments<'_>, since: std::time::Instant) {
    let elapsed = since.elapsed();
    if elapsed >= ANNOUNCE_AFTER {
        eprintln!(
            "cortex-uvm-console: {what} ready in {:.1}s",
            elapsed.as_secs_f64()
        );
    }
}

/// The SHA-256 of a file, as lowercase hex.
async fn digest(path: &Path) -> anyhow::Result<String> {
    let hashed = path.to_path_buf();
    let hash = tokio::task::spawn_blocking(move || -> io::Result<String> {
        use sha2::{Digest as _, Sha256};
        let mut file = std::fs::File::open(&hashed)?;
        let mut hasher = Sha256::new();
        io::copy(&mut file, &mut hasher)?;
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .map_err(|e| anyhow::anyhow!("hashing {}: {e}", path.display()))?;

    hash.map_err(|e| anyhow::anyhow!("hashing {}: {e}", path.display()))
}

/// Read a gzipped rootfs tarball into a file tree.
///
/// The tree and not an image: what a base is made of is a layer, and turning a tree into one
/// is the layer store's. This half is here because it is the tarball's — which compression,
/// which limits — and `assets` is what owns the tarball.
///
/// Announced, because it is minutes of CPU on a cold cache and there is nothing else on
/// stderr to say a session is still coming up.
pub async fn ingest(tarball: &Path) -> anyhow::Result<microsandbox_image::tree::FileTree> {
    eprintln!(
        "cortex-uvm-console: reading {}",
        tarball.file_name().unwrap_or_default().to_string_lossy()
    );
    let started = std::time::Instant::now();

    let file = tokio::fs::File::open(tarball).await?;
    let ingested = ingest_compressed_tar(file, Compression::Gzip, &ResourceLimits::default(), None)
        .await
        .map_err(|e| anyhow::anyhow!("reading {}: {e:?}", tarball.display()))?;

    took(
        &format_args!(
            "{}",
            tarball.file_name().unwrap_or_default().to_string_lossy()
        ),
        started,
    );
    Ok(ingested.tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The libkrunfw tag and the `microsandbox-*` crates move together — see [`KRUNFW_TAG`].
    #[test]
    fn the_kernel_comes_from_the_release_the_microsandbox_crates_do() {
        let lock =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../Cargo.lock"))
                .unwrap();
        let version = lock
            .split("[[package]]")
            .find(|package| package.contains("name = \"microsandbox-network\""))
            .and_then(|package| {
                package
                    .lines()
                    .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
            })
            .expect("microsandbox-network in the lockfile");
        assert_eq!(
            KRUNFW_TAG,
            format!("v{version}"),
            "bump KRUNFW_TAG and its digests with the microsandbox crates"
        );
    }

    /// A download is kept only when it hashes to the pin, and a kept one is not fetched again.
    #[tokio::test]
    async fn a_pinned_download_is_checked_before_it_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("published");
        std::fs::write(&source, b"a kernel").unwrap();
        let url = format!("file://{}", source.display());
        let right = {
            use sha2::{Digest as _, Sha256};
            format!("{:x}", Sha256::digest(b"a kernel"))
        };
        let dest = dir.path().join("lib").join("kernel");
        let what = format_args!("test");

        let e = fetch_pinned(&url, &"0".repeat(64), &dest, &what)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not the pinned"), "{e}");
        assert!(!dest.exists());
        assert_eq!(
            std::fs::read_dir(dir.path().join("lib")).unwrap().count(),
            0,
            "a refused download leaves nothing behind"
        );

        assert_eq!(
            fetch_pinned(&url, &right, &dest, &what).await.unwrap(),
            dest
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"a kernel");

        // Kept: with the source gone, the copy is what answers.
        std::fs::remove_file(&source).unwrap();
        assert_eq!(
            fetch_pinned(&url, &right, &dest, &what).await.unwrap(),
            dest
        );
    }
}
