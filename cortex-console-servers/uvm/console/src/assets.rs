//! The four things a boot needs on disk, and where each one comes from.
//!
//! | | lifetime | cost |
//! |---|---|---|
//! | the libkrunfw kernel | installed | found, never built |
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
    erofs::write_erofs,
    ext4::{Ext4FormatOptions, format_ext4},
    tar::{Compression, ingest_compressed_tar},
    tree::ResourceLimits,
};

use crate::contract::{BaseFormat, GUEST_BIN_PATH, IMAGE_SPEC_PATH, ImageSpec};

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
/// `$HOME/.cortex/uvm`.
pub fn home() -> io::Result<PathBuf> {
    std::env::var_os("CORTEX_UVM_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cortex/uvm")))
        .ok_or_else(|| io::Error::other("neither CORTEX_UVM_HOME nor HOME is set"))
}

/// Find the libkrunfw kernel: `CORTEX_UVM_KERNEL`, else wherever a package manager put it.
///
/// Not downloaded. A kernel is loaded into the guest's memory by a `dlopen`ed library that
/// has to be signed compatibly with the process loading it, so where it came from is a
/// property of the installation rather than something a console server should decide on
/// its own — and getting it wrong fails at `VmCreate`, well after the point where a
/// helpful message is easy to give.
///
/// Both spellings of the name are checked: the versioned one a release ships, and the
/// plain one a package manager symlinks.
pub fn resolve_kernel() -> anyhow::Result<PathBuf> {
    if let Some(kernel) = std::env::var_os("CORTEX_UVM_KERNEL") {
        return Ok(PathBuf::from(kernel));
    }

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

    candidates.into_iter().find(|p| p.exists()).ok_or_else(|| {
        anyhow::anyhow!(
            "no libkrunfw found. Install it (`brew install libkrunfw`, or microsandbox) \
             or set CORTEX_UVM_KERNEL to the library's path"
        )
    })
}

/// A read-only base image, and the two things about it that are not the path.
pub struct BaseImage {
    /// The image on this host.
    pub path: PathBuf,

    /// What a boot has to attach it as.
    pub format: BaseFormat,

    /// What the image says about running a process in it. Empty for a base that came with
    /// no such statement, which is every base that is not an OCI image.
    pub spec: ImageSpec,
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

/// The read-only base image every session overlays, provisioning it once if it is not
/// cached.
///
/// Three sources, in the order a client's own answer beats a server's:
///
/// Two sources, and the choice between them was made at `init`:
///
/// - `reference` — an OCI image, pulled and materialized (see [`pull`]). What the session named,
///   or what [`IMAGE_ENV`] named for a session that named nothing; the server settled which
///   before calling here.
/// - `None` — the pinned rootfs tarball, encoded to an EROFS (see [`Rootfs`]).
pub async fn base_image(reference: Option<&str>) -> anyhow::Result<BaseImage> {
    if let Some(reference) = reference {
        return pull(reference).await;
    }

    let rootfs = Rootfs::host();
    let image = home()?.join("images").join(rootfs.image_name());
    if !image.exists() {
        let tarball = rootfs.fetch().await?;
        encode_erofs(&tarball, &image).await?;
    }

    Ok(BaseImage {
        path: image,
        format: BaseFormat::Raw,
        spec: ImageSpec::default(),
    })
}

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
async fn pull(reference: &str) -> anyhow::Result<BaseImage> {
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

    let path = cache.vmdk_path(&pulled.manifest_digest);
    anyhow::ensure!(
        path.exists(),
        "pulling {reference} left no image at {}",
        path.display()
    );

    Ok(BaseImage {
        path,
        format: BaseFormat::Vmdk,
        spec: ImageSpec {
            env: pulled.config.env,
            working_dir: pulled.config.working_dir,
        },
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
struct Rootfs {
    url: &'static str,
    /// Lowercase hex SHA-256 of the tarball.
    sha256: &'static str,
    /// Names the cached image, so a change to the pin above is a different cache entry
    /// rather than a stale one.
    name: &'static str,
}

impl Rootfs {
    fn host() -> Rootfs {
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

    fn image_name(&self) -> String {
        format!("{}.erofs", self.name)
    }

    /// The tarball on disk, downloading and verifying it if it is not cached.
    async fn fetch(&self) -> anyhow::Result<PathBuf> {
        let dest = home()?.join("rootfs").join(format!("{}.tar.gz", self.name));
        if dest.exists() && digest(&dest).await? == self.sha256 {
            return Ok(dest);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Downloaded to a temp path and renamed, so an interrupted download is never
        // mistaken for a cached one. `curl` rather than an HTTP client, because this is
        // the only request this crate makes and a TLS stack is a large thing to link for
        // one GET.
        let tmp = dest.with_extension("download");
        eprintln!("cortex-uvm-console: downloading {}", self.url);
        let started = std::time::Instant::now();
        let status = tokio::process::Command::new("curl")
            .arg("-fsSL")
            .arg(self.url)
            .arg("-o")
            .arg(&tmp)
            .status()
            .await
            .map_err(|e| anyhow::anyhow!("running curl: {e}"))?;
        anyhow::ensure!(
            status.success(),
            "downloading {} failed ({status})",
            self.url
        );

        let got = digest(&tmp).await?;
        if got != self.sha256 {
            let _ = std::fs::remove_file(&tmp);
            anyhow::bail!(
                "{} hashed to {got}, not the pinned {}",
                self.url,
                self.sha256
            );
        }

        std::fs::rename(&tmp, &dest)?;
        took(&format_args!("{}", self.name), started);
        Ok(dest)
    }
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

/// Turn a gzipped rootfs tarball into a read-only EROFS image.
///
/// Written to a temp path and renamed, so two consoles provisioning the same base image at
/// once cannot see a half-encoded one: both do the work, and the rename decides which
/// copy survives.
async fn encode_erofs(tarball: &Path, image: &Path) -> anyhow::Result<()> {
    if let Some(parent) = image.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Announced like the download above, and for the same reason: it is minutes of CPU on a
    // cold cache and there is nothing else on stderr to say a session is still coming up.
    eprintln!(
        "cortex-uvm-console: encoding {}",
        tarball.file_name().unwrap_or_default().to_string_lossy()
    );
    let started = std::time::Instant::now();

    let file = tokio::fs::File::open(tarball).await?;
    let ingested = ingest_compressed_tar(file, Compression::Gzip, &ResourceLimits::default(), None)
        .await
        .map_err(|e| anyhow::anyhow!("reading {}: {e:?}", tarball.display()))?;

    let tmp = unique(
        image.parent().unwrap_or(Path::new(".")).to_path_buf(),
        "base.erofs",
    );
    let (tree, encode_to) = (ingested.tree, tmp.clone());
    tokio::task::spawn_blocking(move || write_erofs(&tree, &encode_to))
        .await
        .map_err(|e| anyhow::anyhow!("encoding the base image: {e}"))?
        .map_err(|e| anyhow::anyhow!("encoding the base image: {e:?}"))?;

    std::fs::rename(&tmp, image)?;
    took(
        &format_args!(
            "{}",
            image.file_name().unwrap_or_default().to_string_lossy()
        ),
        started,
    );
    Ok(())
}
