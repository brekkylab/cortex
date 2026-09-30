//! Fetching the console server when this host has none.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::cache_root;

/// Where releases are fetched from unless `$CORTEX_DIST_URL` says otherwise.
const DIST_URL: &str = "https://cortex-dist-044443350235-us-east-1-an.s3.us-east-1.amazonaws.com";

/// Fetch the console server into [`cache_root`]`/bin` if absent, and return that directory.
///
/// **Present is enough**: a `bin/` that already has `cortex-krun` (from here or from
/// `cargo xtask install`) is left alone; this does not keep it up to date.
///
/// The version is `$CORTEX_KRUN_VERSION` if set, else this platform's `latest`. `cortex-krun`
/// is placed last, so a partial fetch leaves a `bin/` the next call refetches rather than one
/// that looks complete.
///
/// # Release layout
///
/// cortex-krun's `cargo xtask upload` publishes one archive per platform holding what its
/// `cargo xtask install` puts in `bin/`: `cortex-krun`, the VM process, the guest, the guest
/// kernel, and `abin/`. Under the bucket's public HTTPS endpoint:
///
/// ```text
/// cortex-krun/<os>-<arch>/latest                                    one line: a version
/// cortex-krun/<os>-<arch>/<version>/cortex-krun-<os>-<arch>.tar.gz
/// ```
///
/// `<os>`/`<arch>` are as [`std::env::consts`] spells them. A version is a cortex-krun git sha;
/// `latest` is per platform because each platform is built and uploaded by its own machine.
pub async fn ensure_cortex() -> anyhow::Result<PathBuf> {
    let root = cache_root();
    let bin = root.join("bin");
    let server = format!("cortex-krun{}", std::env::consts::EXE_SUFFIX);
    if bin.join(&server).is_file() {
        return Ok(bin);
    }

    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let base = std::env::var("CORTEX_DIST_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DIST_URL.to_string());
    let base = format!("{}/cortex-krun/{platform}", base.trim_end_matches('/'));

    let version = match std::env::var("CORTEX_KRUN_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
    {
        Some(version) => version,
        None => {
            let latest = fetch(&format!("{base}/latest"))
                .await
                .with_context(|| format!("no cortex-krun release is published for {platform}"))?;
            String::from_utf8(latest)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .with_context(|| format!("{base}/latest does not name a version"))?
        }
    };

    let url = format!("{base}/{version}/cortex-krun-{platform}.tar.gz");
    let archive = fetch(&url).await?;
    tokio::task::spawn_blocking(move || unpack(&archive, &root, &server))
        .await
        .context("unpacking cortex-krun panicked")??;
    Ok(bin)
}

/// GET `url`, whole. A bucket without public listing answers a missing key with 403, not 404,
/// so a failure reports only the URL and status, without interpreting it.
async fn fetch(url: &str) -> anyhow::Result<Vec<u8>> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("fetching {url}"))?;
    let status = response.status();
    anyhow::ensure!(status.is_success(), "fetching {url}: {status}");
    let body = response
        .bytes()
        .await
        .with_context(|| format!("reading {url}"))?;
    Ok(body.to_vec())
}

/// Unpack `archive` beside `bin/` and move its entries in, `server` last.
///
/// The staging directory is per-process and on the same filesystem, so every move is a rename
/// and no reader of `bin/` sees a half-written file.
fn unpack(archive: &[u8], root: &Path, server: &str) -> anyhow::Result<()> {
    let part = root.join(format!(".bin.{}.part", std::process::id()));
    let _ = std::fs::remove_dir_all(&part);
    std::fs::create_dir_all(&part).with_context(|| format!("creating {}", part.display()))?;
    let moved = (|| {
        tar::Archive::new(flate2::read::GzDecoder::new(archive))
            .unpack(&part)
            .context("unpacking the cortex-krun archive")?;
        anyhow::ensure!(
            part.join(server).is_file(),
            "the cortex-krun archive has no {server}"
        );

        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
        for entry in std::fs::read_dir(&part)? {
            let entry = entry?;
            if entry.file_name() != server {
                replace(&entry.path(), &bin.join(entry.file_name()))?;
            }
        }
        replace(&part.join(server), &bin.join(server))
    })();
    let _ = std::fs::remove_dir_all(&part);
    moved
}

/// Rename `from` over `to`. A rename replaces a file but not a directory, so an old directory
/// is removed first.
fn replace(from: &Path, to: &Path) -> anyhow::Result<()> {
    if from.is_dir() && to.is_dir() {
        std::fs::remove_dir_all(to).with_context(|| format!("removing {}", to.display()))?;
    }
    std::fs::rename(from, to)
        .with_context(|| format!("moving {} to {}", from.display(), to.display()))
}
