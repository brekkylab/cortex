//! `/abin`: the native executables a session can run.
//!
//! Cortex's own, and only those. A session does not name executables and cannot add any, so
//! the set is the same for every session — which makes `/abin` **one read-only disk shared
//! by all of them**, held in the layer store like any other layer and attached as it stands.
//! There is nothing to assemble per session and nothing to stitch.
//!
//! # Why a release is named by commit
//!
//! One tarball per OS and architecture, under the sha of the commit its executables were
//! built from. A build is therefore named by its source rather than by a version somebody
//! remembered to bump, and `abin/latest` is how a session finds the newest one without being
//! told.
//!
//! **One tarball rather than a file per executable.** Adding one costs no new constant, and a
//! release is atomic: there is no state in which half of it is here.
//!
//! **The bytes are not pinned**, unlike the base rootfs, which carries a SHA-256. What that
//! trades away is argued in the design note rather than here; what it means for this file is
//! that `abin/<sha>/` says what it says only as far as write access to the bucket is
//! controlled, and that [`valid_sha`] is the only check anything performs.
//!
//! # Getting one without the network
//!
//! [`DIR_ENV`] short-circuits everything: it names a directory somebody just built, nothing
//! is downloaded, and nothing is checked. [`VERSION_ENV`] pins a release instead of following
//! the pointer. Failing both, a machine that has downloaded a release before keeps using the
//! newest one it has — see [`newest_cached`]. Only a machine with none of the three boots
//! without `/abin`, which is what every session did before there was anything to fetch.

use std::path::{Path, PathBuf};

use cortex_uvm_console::layer::{Layer, LayerId, LayerStore};
use microsandbox_image::tree::{FileTree, TreeNode};

/// A directory of executables to use instead of the release, as a host path.
///
/// The development and offline path, and the same shape as `CORTEX_UVM_KERNEL` and
/// `CORTEX_UVM_IMAGE`. No download and no digest check: what it names is whatever the person
/// running this just built.
pub const DIR_ENV: &str = "CORTEX_ABIN_DIR";

/// A release to use instead of whatever `latest` names, as a git sha.
///
/// The escape hatch for pinning: a machine that must keep the executables it has while
/// somebody else publishes, or a bisect that wants the ones from a particular commit.
pub const VERSION_ENV: &str = "CORTEX_ABIN_VERSION";

/// Where releases are served from, overriding [`BASE_URL`].
///
/// For tests and for a staging bucket. Without it the download path could only be exercised
/// by a test that reaches the internet.
pub const BASE_URL_ENV: &str = "CORTEX_ABIN_BASE_URL";

/// Where releases are served from.
///
/// The bucket's own endpoint rather than a domain of ours: it is already HTTPS, and a vanity
/// hostname in front of it later changes this constant and moves no object.
const BASE_URL: &str = "https://cortex-dist-044443350235-us-east-1-an.s3.us-east-1.amazonaws.com";

/// A git sha, or nothing.
///
/// Trimmed, then checked for exactly 40 lowercase hex characters. The check is not pedantry:
/// this value comes off the network and goes into both a URL and a path, and a 404 page or a
/// truncated write would otherwise be pasted into one.
fn valid_sha(text: &str) -> Option<&str> {
    let sha = text.trim();
    let ok = sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ok.then_some(sha)
}

/// What this host's tarball is called.
///
/// `linux` is the guest's OS and not this host's — the executables run inside the VM. The
/// segment exists so that a host build, if one is ever needed, is a new name rather than a
/// migration of the old one.
fn tarball_name() -> String {
    format!("abin-linux-{}.tar.gz", std::env::consts::ARCH)
}

fn tarball_url(base: &str, sha: &str) -> String {
    format!("{base}/abin/{sha}/{}", tarball_name())
}

fn base_url() -> String {
    std::env::var(BASE_URL_ENV).unwrap_or_else(|_| BASE_URL.to_string())
}

/// Where downloaded releases are kept — beside `rootfs/`, `layers/` and `built/`.
fn cache_dir() -> anyhow::Result<PathBuf> {
    Ok(crate::assets::home()?.join("abin"))
}

/// Where one release's tarball goes.
///
/// The host's name is in it as well as the release's, because one `CORTEX_UVM_HOME` can be
/// shared across architectures — an NFS home, a mounted volume — and two of them writing
/// `<sha>.tar.gz` would each serve the other's executables.
fn cached_path(dir: &Path, sha: &str) -> PathBuf {
    dir.join(format!("{sha}-{}", tarball_name()))
}

/// The most recently downloaded release for this host, if there is one.
///
/// The offline answer. Newest by mtime, which is when the rename that completed the download
/// landed — a partially written file is never named this way, so anything matching here is
/// whole.
fn newest_cached(dir: &Path) -> Option<PathBuf> {
    let suffix = tarball_name();
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let matches = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(&suffix) && name.len() > suffix.len());
        if !matches {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(seen, _)| modified > *seen) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

/// How long the pointer lookup may take before a session stops waiting for it.
///
/// Short on purpose: this is one small object, it is on the path of every boot that needs
/// `/abin`, and the answer to not getting it is the cache rather than a failure.
const POINTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long a tarball download may take. Generous — it is megabytes — and present only so
/// that a hung connection cannot hold a boot open indefinitely.
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Which release `latest` names, or `None` if it cannot be had.
///
/// **Every failure is `None`.** Unreachable, timed out, a 404, an error document served with
/// a 200 — none of them are worth a different answer, because the caller's response to all
/// of them is the same: use what is cached.
async fn resolve(base: &str) -> Option<String> {
    let url = format!("{base}/abin/latest");
    let read = tokio::process::Command::new("curl")
        .arg("-fsSL")
        .arg(&url)
        // Without this the timeout below drops the future and leaves the `curl` running:
        // tokio reaps a dropped child but does not signal it. On a slow network that is one
        // orphan per boot, forever.
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(POINTER_TIMEOUT, read)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    valid_sha(&text).map(str::to_string)
}

/// This host's tarball for the release it should be running, downloading it if needed.
///
/// The order is [`DIR_ENV`] (handled by the caller, not here), then [`VERSION_ENV`], then
/// the pointer, then whatever is already downloaded.
async fn fetch() -> anyhow::Result<PathBuf> {
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir)?;
    let base = base_url();

    let named = match std::env::var(VERSION_ENV) {
        Ok(pinned) => Some(
            valid_sha(&pinned)
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("{VERSION_ENV} is not a git sha: {pinned}"))?,
        ),
        Err(_) => resolve(&base).await,
    };

    let Some(sha) = named else {
        // Nothing said which release to use, so the newest one already here is the answer.
        return newest_cached(&dir).ok_or_else(|| {
            anyhow::anyhow!(
                "could not reach {base}/abin/latest and nothing is cached in {}. Set \
                 {DIR_ENV} to a directory of guest-native executables, or {VERSION_ENV} to a \
                 release to fetch.",
                dir.display()
            )
        });
    };

    let dest = cached_path(&dir, &sha);
    if dest.exists() {
        return Ok(dest);
    }

    // To a temporary name and renamed, so an interrupted download is never mistaken for a
    // complete one — the rule `assets::Rootfs::fetch` follows for the base rootfs.
    //
    // **The pid is in that name, and it has to be.** Two console servers starting at once on
    // a cold cache resolve the same release and both download it; sharing one temporary path
    // means two `curl -o` truncating and writing the same file, and then renaming the
    // interleaved result into the cache. The rootfs survives that because it is verified
    // against a digest afterwards. A release is not, so nothing would ever notice — every
    // later boot would find the corrupt tarball, fail to read it, and go without `/abin`.
    let tmp = dest.with_extension(format!("download.{}", std::process::id()));
    let url = tarball_url(&base, &sha);
    eprintln!("cortex-uvm-console: downloading {url}");
    let run = tokio::process::Command::new("curl")
        .arg("-fsSL")
        .arg(&url)
        .arg("-o")
        .arg(&tmp)
        // As in `resolve`: the timeout below drops this future, and a `curl` that was not
        // killed would carry on writing to `tmp` after this function has given up on it.
        .kill_on_drop(true)
        .status();
    let timed_out = tokio::time::timeout(DOWNLOAD_TIMEOUT, run).await;
    // Every way out of here that is not a rename removes the temporary — including the
    // timeout, which used to leave a partial file behind under a name nothing collects.
    let status = match timed_out {
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            anyhow::bail!("downloading {url} took longer than {DOWNLOAD_TIMEOUT:?}");
        }
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(&tmp);
            anyhow::bail!("running curl: {e}");
        }
        Ok(Ok(status)) => status,
    };
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("downloading {url} failed ({status})");
    }

    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// The disk `/abin` is: cortex's own executables, as one layer.
///
/// `builtin` is [`DIR_ENV`] when it is set, and read by the caller rather than here: what a
/// server is configured with is read where the rest of its configuration is read, and a
/// function that reached for it itself would be one no test could call twice.
///
/// The layer is handed over as it stands, which is why there is no stitch and no assembled
/// disk anywhere — a base layer carries no whiteouts, so it is already a valid image, and
/// the store already keeps one copy of it however many sessions ask.
///
/// **Async, because getting a release is**: a download and then an `ingest_compressed_tar`.
/// The encode that follows is not, and is not moved off the runtime either — the same shape
/// [`crate::base`]'s pinned rootfs already has, where the tree being encoded is a whole
/// Alpine image rather than two executables.
pub async fn disk(builtin: Option<&Path>, store: &LayerStore) -> anyhow::Result<PathBuf> {
    match builtin {
        Some(dir) => {
            anyhow::ensure!(
                dir.is_dir(),
                "{DIR_ENV} is {}, which is not a directory",
                dir.display()
            );
            Ok(layer_of(dir, store)?.erofs)
        }
        None => {
            let tarball = fetch().await?;
            disk_from_tarball(&tarball, store).await
        }
    }
}

/// One release tarball as a layer.
///
/// Named by the bytes of the tarball, which is what lets the whole of the work below be
/// skipped on every boot after the first: the id is known from a file this host already has,
/// so a store that has it never reads the archive at all.
///
/// Read straight into a tree rather than unpacked to a directory first — `crate::assets`'s
/// `ingest` is the same call the pinned rootfs makes, and going through the filesystem would
/// add a half-written directory to worry about for no gain.
async fn disk_from_tarball(tarball: &Path, store: &LayerStore) -> anyhow::Result<PathBuf> {
    let id = LayerId::of(&std::fs::read(tarball)?);
    if store.has(&id) {
        return Ok(store.get(&id)?.erofs);
    }
    // A tarball that will not read is thrown away rather than kept. Nothing verifies a
    // release, so a file that arrived damaged looks exactly like a good one to every later
    // boot: `fetch` would find it cached, hand it back, and this would fail again. Deleting
    // it is what turns "no `/abin` on this host, permanently" into "no `/abin` this once".
    let tree = match crate::assets::ingest(tarball).await {
        Ok(tree) => tree,
        Err(e) => {
            let _ = std::fs::remove_file(tarball);
            return Err(e.context(format!(
                "{} could not be read and has been discarded; the next session will fetch it \
                 again",
                tarball.display()
            )));
        }
    };
    Ok(store.put(&id, &tree)?.erofs)
}

/// One directory as a layer, named by what is in it.
///
/// Content-addressed rather than named for the path, so that a directory somebody is
/// rebuilding gets a new layer each time its contents change and the same one when they do
/// not. The tree is read once and hashed from what was read, rather than walking the
/// directory twice.
fn layer_of(dir: &Path, store: &LayerStore) -> anyhow::Result<Layer> {
    let tree = cortex_uvm_console::layer::tree::from_dir(dir)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", dir.display()))?;
    let id = digest(&tree)?;
    store.put(&id, &tree)
}

/// A tree's identity: everything in it, in one order.
///
/// Mode and contents as well as names, because two directories differing only in whether
/// something is executable are two different `/abin`s.
fn digest(tree: &FileTree) -> anyhow::Result<LayerId> {
    use sha2::{Digest as _, Sha256};

    fn feed(node: &TreeNode, path: &[u8], hasher: &mut Sha256) -> anyhow::Result<()> {
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path);
        match node {
            TreeNode::RegularFile(file) => {
                hasher.update(b"f");
                hasher.update(file.metadata.mode.to_le_bytes());
                let data = file.data.read_all()?;
                hasher.update((data.len() as u64).to_le_bytes());
                hasher.update(&data);
            }
            TreeNode::Symlink(link) => {
                hasher.update(b"l");
                hasher.update((link.target.len() as u64).to_le_bytes());
                hasher.update(&link.target);
            }
            TreeNode::Directory(directory) => {
                hasher.update(b"d");
                hasher.update(directory.metadata.mode.to_le_bytes());
                // `entries` is a `BTreeMap`, so this is name order and therefore the same
                // order every time the same directory is read.
                for (name, child) in &directory.entries {
                    let mut child_path = path.to_vec();
                    if !child_path.is_empty() {
                        child_path.push(b'/');
                    }
                    child_path.extend_from_slice(name.as_encoded_bytes());
                    feed(child, &child_path, hasher)?;
                }
            }
            // Nothing else belongs in an `/abin` and `from_dir` leaves them out, but a
            // digest is only sound if it accounts for everything a tree can hold.
            TreeNode::CharDevice(device) => {
                hasher.update(b"c");
                hasher.update(device.major.to_le_bytes());
                hasher.update(device.minor.to_le_bytes());
            }
            TreeNode::BlockDevice(device) => {
                hasher.update(b"b");
                hasher.update(device.major.to_le_bytes());
                hasher.update(device.minor.to_le_bytes());
            }
            TreeNode::Fifo(metadata) => {
                hasher.update(b"p");
                hasher.update(metadata.mode.to_le_bytes());
            }
            TreeNode::Socket(metadata) => {
                hasher.update(b"s");
                hasher.update(metadata.mode.to_le_bytes());
            }
        }
        Ok(())
    }

    let mut hasher = Sha256::new();
    hasher.update(b"cortex abin tree v1\n");
    feed(&TreeNode::Directory(tree.root.clone()), b"", &mut hasher)?;
    LayerId::parse(&format!("sha256:{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pointer is 40 lowercase hex and nothing else. It arrives over the network and is
    /// then put into a URL and a filename, so it is the one value here that is not taken at
    /// its word — and with no digest behind the tarball, this is the only check there is.
    #[test]
    fn only_a_real_sha_is_accepted_as_a_pointer() {
        let good = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(valid_sha(good), Some(good));
        assert_eq!(valid_sha(&format!("{good}\n")), Some(good));

        assert_eq!(valid_sha(""), None);
        assert_eq!(
            valid_sha("0123456789ABCDEF0123456789abcdef01234567"),
            None,
            "uppercase"
        );
        assert_eq!(valid_sha(&good[..39]), None, "too short");
        assert_eq!(valid_sha(&format!("{good}0")), None, "too long");
        assert_eq!(valid_sha("../../etc/passwd"), None);
        assert_eq!(valid_sha("<!doctype html><html>404"), None, "an error page");
    }

    /// The name carries the OS as well as the architecture. Only `linux` is published today
    /// — the guest is the only consumer — and the segment is there so that adding a host
    /// build later does not mean re-publishing what already exists.
    #[test]
    fn a_tarball_is_named_for_this_os_and_architecture() {
        let name = tarball_name();
        assert!(name.starts_with("abin-linux-"), "{name}");
        assert!(name.contains(std::env::consts::ARCH), "{name}");
        assert!(name.ends_with(".tar.gz"), "{name}");
    }

    /// A cached tarball is named for its release *and* for this host, so two architectures
    /// sharing a home directory do not overwrite each other.
    /// A pointer nobody can reach is not an error — it is the cache's turn. This is the
    /// rule that keeps a machine that has booted once working on a plane.
    #[tokio::test]
    async fn an_unreachable_pointer_is_not_an_error() {
        // A port nothing is listening on, so this fails fast rather than timing out.
        assert_eq!(resolve("http://127.0.0.1:1").await, None);
    }

    /// And a pointer that answers with something that is not a sha is treated the same way
    /// — an S3 error document is still a 200 to something.
    #[tokio::test]
    async fn a_pointer_that_is_not_a_sha_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("abin")).unwrap();
        std::fs::write(
            dir.path().join("abin/latest"),
            b"<Error><Code>NoSuchKey</Code>",
        )
        .unwrap();
        let base = format!("file://{}", dir.path().display());
        assert_eq!(resolve(&base).await, None);
    }

    /// A pointer that answers properly is the release to use. `file://` works because the
    /// download is `curl`, which reads it like any other URL — which is also what makes
    /// this path testable without a bucket or an HTTP server.
    #[tokio::test]
    async fn a_pointer_that_names_a_sha_is_used() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("abin")).unwrap();
        std::fs::write(dir.path().join("abin/latest"), format!("{sha}\n")).unwrap();
        let base = format!("file://{}", dir.path().display());
        assert_eq!(resolve(&base).await.as_deref(), Some(sha));
    }

    #[test]
    fn a_cached_tarball_is_named_for_its_release_and_this_host() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let path = cached_path(Path::new("/home/abin"), sha);
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(sha), "{name}");
        assert!(name.ends_with(&tarball_name()), "{name}");
    }

    /// With no pointer to resolve, the newest download is what a session gets — which is
    /// what keeps a machine that has booted once working with no network at all.
    #[test]
    fn the_newest_download_is_the_offline_fallback() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            newest_cached(dir.path()).is_none(),
            "an empty cache offered something"
        );

        let old = cached_path(dir.path(), "1111111111111111111111111111111111111111");
        let new = cached_path(dir.path(), "2222222222222222222222222222222222222222");
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();

        // Distinct mtimes without sleeping: set the older one back explicitly. A test that
        // slept a second to make two timestamps differ would be a second slower for nothing.
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(hour_ago)
            .unwrap();

        assert_eq!(newest_cached(dir.path()), Some(new));
    }

    /// Somebody else's tarball in the same directory is not ours to boot from.
    #[test]
    fn another_hosts_tarball_is_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("abin-linux-s390x.tar.gz"), b"not ours").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"not a tarball").unwrap();
        assert_eq!(newest_cached(dir.path()), None);
    }

    #[test]
    fn a_url_is_the_base_then_the_sha_then_the_name() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            tarball_url("https://example.test", sha),
            format!("https://example.test/abin/{sha}/{}", tarball_name())
        );
    }

    fn dir_of(names: &[&str]) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for name in names {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\necho {name}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    fn store_in(dir: &Path) -> LayerStore {
        LayerStore::open(&dir.join("layers")).unwrap()
    }

    #[tokio::test]
    async fn a_local_directory_stands_in_for_the_release() {
        let builtin = dir_of(&["mem", "index"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let layer = layer_of(builtin.path(), &store).unwrap();

        let tree = layer
            .tree(cortex_uvm_console::layer::tree::Contents::Skip)
            .unwrap();
        assert!(tree.get(b"mem").is_some(), "the override was not used");
        assert!(tree.get(b"index").is_some());

        // And through the entry point a boot uses, which is the one that has to stay async.
        let disk = disk(Some(builtin.path()), &store).await.unwrap();
        assert_eq!(disk, layer.erofs);
    }

    /// A tarball that will not read is deleted, so the next session downloads it again.
    ///
    /// The one that matters most, because nothing verifies a release: a file that arrived
    /// damaged is indistinguishable from a good one to `fetch`, which would hand the same
    /// broken bytes back on every boot from here on. Deleting it is the whole of the
    /// recovery.
    #[tokio::test]
    async fn a_tarball_that_will_not_read_is_not_kept() {
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let cache = tempfile::tempdir().unwrap();
        let tarball = cached_path(cache.path(), "3333333333333333333333333333333333333333");
        std::fs::write(&tarball, b"this is not a gzipped tar").unwrap();

        let refused = disk_from_tarball(&tarball, &store)
            .await
            .expect_err("a layer came out of nonsense");
        assert!(
            format!("{refused:#}").contains("discarded"),
            "the caller is not told it was thrown away: {refused:#}"
        );
        assert!(
            !tarball.exists(),
            "a tarball that cannot be read was left in the cache to fail again"
        );
    }

    /// A release tarball becomes a layer, and the same tarball twice is the same layer —
    /// which is what makes the second boot on a host free.
    ///
    /// Note what this does *not* assert. A directory and a tarball holding the same
    /// executables give **different** layer ids: the override path names a layer by the
    /// content of the tree it read, and the release path by the bytes of the archive. Ids
    /// name store entries; nothing requires two ways in to agree on one.
    #[tokio::test]
    async fn a_tarball_becomes_a_layer_and_is_not_encoded_twice() {
        let built = dir_of(&["mem", "index"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let cache = tempfile::tempdir().unwrap();
        let tarball = cache.path().join("abin-linux-test.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("czf")
            .arg(&tarball)
            .arg("-C")
            .arg(built.path())
            .arg("mem")
            .arg("index")
            .status()
            .unwrap();
        assert!(status.success(), "tarring the fixture");

        let first = disk_from_tarball(&tarball, &store).await.unwrap();
        assert!(first.exists(), "no disk was written");

        let again = disk_from_tarball(&tarball, &store).await.unwrap();
        assert_eq!(first, again, "one tarball named two layers");
    }

    /// The store is content-addressed, so the same directory has to name the same layer and
    /// a changed one a different layer — otherwise a rebuilt executable would never arrive.
    #[test]
    fn a_directory_names_a_layer_by_what_is_in_it() {
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let first = dir_of(&["mem"]);
        let same = layer_of(first.path(), &store).unwrap().id;
        assert_eq!(layer_of(first.path(), &store).unwrap().id, same);

        std::fs::write(first.path().join("mem"), b"#!/bin/sh\necho changed\n").unwrap();
        assert_ne!(
            layer_of(first.path(), &store).unwrap().id,
            same,
            "a changed executable named the same layer"
        );
    }

    /// The disk is the layer as it stands — an EROFS out of the store, not a descriptor.
    /// A session runs cortex's own executables and no others, so there is nothing to stitch.
    #[tokio::test]
    async fn the_disk_is_the_builtin_layer_itself() {
        let builtin = dir_of(&["mem"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        let disk = disk(Some(builtin.path()), &store).await.unwrap();
        assert_eq!(disk.extension().unwrap(), "erofs");
        assert_eq!(
            disk,
            layer_of(builtin.path(), &store).unwrap().erofs,
            "the disk is not the layer the store holds"
        );
    }

    /// And so every session on this host gets the same file, rather than one assembled apiece.
    #[tokio::test]
    async fn every_session_gets_the_same_disk() {
        let builtin = dir_of(&["mem", "index"]);
        let work = tempfile::tempdir().unwrap();
        let store = store_in(work.path());

        assert_eq!(
            disk(Some(builtin.path()), &store).await.unwrap(),
            disk(Some(builtin.path()), &store).await.unwrap()
        );
    }
}
