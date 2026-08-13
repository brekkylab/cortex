//! The five things a boot needs on disk, and where each one comes from.
//!
//! | | lifetime | cost |
//! |---|---|---|
//! | the libkrunfw kernel | installed | found, never built |
//! | the base image (EROFS) | cached, shared | built once per rootfs, from a tarball |
//! | the session image (ext4) | one session | formatted per boot, sparse |
//! | the boot root | one session | a directory with one file in it |
//! | the spec file (BSON) | one session, or less | a few kilobytes, `0600`, unlinked on sight |
//!
//! The split down the middle of that table is the point. Two of them are shared and
//! expensive and live under [`home`]; three are this session's, live in a temp directory,
//! and are deleted when the value holding them drops. Nothing a session writes can reach
//! anything another session reads — which is the whole reason the guest boots onto an
//! overlay instead of onto a writable directory, and the reason the spec file is here
//! rather than anywhere the guest can see.
//!
//! # The base image, and what is deliberately not here
//!
//! A rootfs tarball becomes an EROFS: `ingest_compressed_tar` reads it into a file tree,
//! `write_erofs` encodes that tree as a read-only image. No `mkfs`, no privileges, no
//! loop device — which is what makes it work the same way on a laptop and in CI.
//!
//! What is *not* here is a registry client. `alpine` arrives as the tarball the
//! distribution publishes, verified against a pinned digest, and that is the whole of the
//! provisioning story. Pulling `python:3.12-slim` from a registry is the same three calls
//! with a manifest fetch in front — [`base_image`] is where it would go, and
//! [`BASE_IMAGE_ENV`] is how a caller who has already built one says so today.
//!
//! [`BASE_IMAGE_ENV`]: crate::contract::BASE_IMAGE_ENV

use std::io;
use std::path::{Path, PathBuf};

use microsandbox_image::erofs::write_erofs;
use microsandbox_image::ext4::{Ext4FormatOptions, format_ext4};
use microsandbox_image::tar::{Compression, ingest_compressed_tar};
use microsandbox_image::tree::ResourceLimits;

use crate::contract::{BASE_IMAGE_ENV, GUEST_BIN_PATH, SESSION_IMAGE_ENV};

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

/// The read-only base image every session overlays, provisioning it once if it is not
/// cached.
///
/// `CORTEX_UVM_BASE_IMAGE` takes it as given, which is the escape hatch for an image built
/// some other way — a registry pull, a `mkfs.erofs`, an image shipped with a product.
pub async fn base_image() -> anyhow::Result<PathBuf> {
    if let Some(image) = std::env::var_os(BASE_IMAGE_ENV) {
        return Ok(PathBuf::from(image));
    }

    let rootfs = Rootfs::host();
    let image = home()?.join("images").join(rootfs.image_name());
    if image.exists() {
        return Ok(image);
    }

    let tarball = rootfs.fetch().await?;
    encode_erofs(&tarball, &image).await?;
    Ok(image)
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

/// This session's namespace, written where the boot child can read it.
///
/// Owns the file: dropping this unlinks it. The boot child unlinks it too, as soon as it
/// has read it — a spec on disk is credentials on disk, and the child is the last reader.
/// Both removals are best-effort for that reason; the first one to succeed is the one that
/// mattered.
///
/// Deliberately **not** under [`BootRoot`], which libkrun serves to the guest as its root
/// filesystem: that would put the credentials on the far side of the boundary the VM is
/// here to be.
pub struct SpecFile {
    path: PathBuf,
}

impl SpecFile {
    pub fn create(spec: &cortex::volume::WorkspaceSpec) -> anyhow::Result<SpecFile> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;

        let path = unique(std::env::temp_dir(), "spec.bson");
        let encoded = bson::serialize_to_vec(spec)
            .map_err(|e| anyhow::anyhow!("encoding the namespace: {e}"))?;

        // 0600 at creation and not afterwards: a `set_permissions` later leaves a window in
        // which the file exists and is readable, which is the whole thing being avoided.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("creating {}: {e}", path.display()))?;
        file.write_all(&encoded)
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;

        Ok(SpecFile { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SpecFile {
    fn drop(&mut self) {
        // Gone already is the expected case, not a failure: the boot child unlinks it the
        // moment it has read it.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The directory libkrun serves as the guest's virtio-fs root, holding the guest binary
/// and nothing else.
///
/// It is a real root only for the moment between the kernel handing over and the guest
/// pivoting onto the overlay — long enough to exec one file. Which is why it is a fresh
/// directory per session rather than something shared: it is writable by the guest for
/// that moment, and two guests sharing one would be two guests writing the same tree.
pub struct BootRoot {
    path: PathBuf,
}

impl BootRoot {
    pub fn create() -> anyhow::Result<BootRoot> {
        use std::os::unix::fs::PermissionsExt;

        let path = unique(std::env::temp_dir(), "boot");
        std::fs::create_dir_all(&path)?;
        let root = BootRoot { path };

        let guest = root.path.join(GUEST_BIN_PATH.trim_start_matches('/'));
        std::fs::write(&guest, GUEST_BIN)?;
        std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o755))?;
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
/// What it cannot cover is `SIGKILL`, and two things are lost by it, not one: a sparse image
/// that may hold everything a session wrote, and a [`SpecFile`] that may hold credentials.
///
/// The second is the sharper one, and this sweep is the only thing that ever removes it in
/// that case. The boot child normally unlinks the spec the moment it has read it, so the
/// exposure needs the child to have not got that far — a machine that lost power between the
/// write and the read, a child that never started. Then the file stays until some later
/// `cortex-uvm-console` starts on the same host and runs this. If none ever does, it stays.
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
            other => panic!("no base rootfs pinned for a {other} host — set {BASE_IMAGE_ENV}"),
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
        Ok(dest)
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::volume::{VolumeSpec, WorkspaceSpec};

    /// The file a boot child reads: readable by nobody else, and gone when the session is.
    #[test]
    fn a_spec_file_is_private_and_owns_its_path() {
        use std::os::unix::fs::PermissionsExt;

        let spec = WorkspaceSpec::default().mount(
            "work",
            VolumeSpec::Local {
                host: "/tmp/somewhere".into(),
            },
        );

        let path = {
            let file = SpecFile::create(&spec).expect("writes");
            let path = file.path().to_path_buf();

            // 0600, because the whole reason this is a file and not an environment
            // variable is that a file's reach can be stated.
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);

            // And it round-trips: what the boot child will read is what was declared.
            let read: WorkspaceSpec =
                bson::deserialize_from_slice(&std::fs::read(&path).unwrap()).expect("parses");
            assert_eq!(read, spec);

            // Named so `sweep_abandoned` can tell whose it is.
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(
                name.starts_with(&format!("{PREFIX}{}-", std::process::id())),
                "{name} carries no pid a later run could read"
            );

            path
        };

        assert!(!path.exists(), "dropping the guard left the spec on disk");
    }

    /// A boot child that read the file may unlink it, and the guard must survive that —
    /// two removals of one path is the normal case, not an error.
    #[test]
    fn a_spec_file_already_unlinked_drops_quietly() {
        let file = SpecFile::create(&WorkspaceSpec::default()).expect("writes");
        std::fs::remove_file(file.path()).expect("the child's unlink");
        drop(file);
    }
}
