//! Fetching the console server when this host has none.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::cache_root;

/// The server release this build fetches by default, set at build time -- a sha or a tag.
/// `None` in a build nobody pinned, which follows `latest`.
const PINNED: Option<&str> = match option_env!("CORTEX_KRUN_PINNED_VERSION") {
    Some(version) if !version.is_empty() => Some(version),
    _ => None,
};

/// Where releases are fetched from unless `$CORTEX_DIST_URL` says otherwise.
const DIST_URL: &str = "https://cortex-dist-044443350235-us-east-1-an.s3.us-east-1.amazonaws.com";

/// Fetch the console server into [`cache_root`]`/bin` if absent, and return that directory.
///
/// **Present is enough**: a `bin/` that already has `cortex-krun` (from here or from
/// `cargo xtask install`) is left alone; this does not keep it up to date.
///
/// The release is `$CORTEX_KRUN_VERSION` if set; else the one this build was pinned to
/// (`CORTEX_KRUN_PINNED_VERSION`, set for a published package so it fetches the server it was
/// tested with); else `latest`. `cortex-krun` is placed last, so a partial fetch leaves a
/// `bin/` the next call refetches rather than one that looks complete.
///
/// # Release layout
///
/// One archive per platform, holding what `bin/` needs, under the bucket's public HTTPS
/// endpoint:
///
/// ```text
/// cortex-krun/<ref>/cortex-krun-<os>-<arch>.tar.gz
/// ```
///
/// `<os>`/`<arch>` are as [`std::env::consts`] spells them. `<ref>` is any name a release goes
/// by (a cortex-krun git sha, a version tag, or `latest`), so fetching is one URL whichever
/// name is given.
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
    let release = std::env::var("CORTEX_KRUN_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| PINNED.map(str::to_string))
        .unwrap_or_else(|| "latest".to_string());

    let url = format!(
        "{}/cortex-krun/{release}/cortex-krun-{platform}.tar.gz",
        base.trim_end_matches('/')
    );
    let archive = fetch(&url).await.with_context(|| {
        format!("no cortex-krun release is published for {platform} as `{release}`")
    })?;
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
