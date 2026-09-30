//! Fetching the console server when this host has none.
//!
//! virtx-uvm's `cargo xtask upload` publishes one archive per platform, holding what its
//! `cargo xtask install` would put in [`cache_root`]`/bin`: `virtx-uvm`, the VM process
//! beside it, the guest, the guest kernel, and `abin/`. The layout it writes, under the
//! bucket's public HTTPS endpoint, is
//!
//! ```text
//! virtx-uvm/<os>-<arch>/latest                                    one line: a version
//! virtx-uvm/<os>-<arch>/<version>/virtx-uvm-<os>-<arch>.tar.gz
//! ```
//!
//! with `<os>` and `<arch>` spelled as [`std::env::consts`] spells them. A version is a git
//! sha of virtx-uvm, and `latest` is per platform because each is built on a machine of
//! its own and uploaded when that machine is done.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::cache_root;

/// Where releases are fetched from unless `$VIRTX_DIST_URL` says otherwise.
const DIST_URL: &str = "https://cortex-dist-044443350235-us-east-1-an.s3.us-east-1.amazonaws.com";

/// Fetch the console server into [`cache_root`]`/bin` if it is not there, and answer that
/// directory.
///
/// **Present is enough.** A `bin/` that already has `virtx-uvm` is left alone, whether it
/// came from here or from `cargo xtask install` -- this makes a host able to run a session,
/// and does not keep one up to date.
///
/// The version is `$VIRTX_UVM_VERSION` if set, else whatever this platform's `latest`
/// names. `virtx-uvm` itself is the last file put in place, so a fetch that fails partway
/// leaves a `bin/` the next call fetches into again rather than one that looks complete.
pub async fn ensure_virtx() -> anyhow::Result<PathBuf> {
    let root = cache_root();
    let bin = root.join("bin");
    let server = format!("virtx-uvm{}", std::env::consts::EXE_SUFFIX);
    if bin.join(&server).is_file() {
        return Ok(bin);
    }

    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let base = std::env::var("VIRTX_DIST_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DIST_URL.to_string());
    let base = format!("{}/virtx-uvm/{platform}", base.trim_end_matches('/'));

    let version = match std::env::var("VIRTX_UVM_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
    {
        Some(version) => version,
        None => {
            let latest = fetch(&format!("{base}/latest"))
                .await
                .with_context(|| format!("no virtx-uvm release is published for {platform}"))?;
            String::from_utf8(latest)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .with_context(|| format!("{base}/latest does not name a version"))?
        }
    };

    let url = format!("{base}/{version}/virtx-uvm-{platform}.tar.gz");
    let archive = fetch(&url).await?;
    tokio::task::spawn_blocking(move || unpack(&archive, &root, &server))
        .await
        .context("unpacking virtx-uvm panicked")??;
    Ok(bin)
}

/// GET `url`, whole. An S3 bucket without public listing answers a missing key with 403
/// rather than 404, so a status is reported as the URL that answered it and nothing more.
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
/// Beside rather than into: a directory of this process's own on the same filesystem, so
/// every move is a rename and no reader of `bin/` sees a file half-written.
fn unpack(archive: &[u8], root: &Path, server: &str) -> anyhow::Result<()> {
    let part = root.join(format!(".bin.{}.part", std::process::id()));
    let _ = std::fs::remove_dir_all(&part);
    std::fs::create_dir_all(&part).with_context(|| format!("creating {}", part.display()))?;
    let moved = (|| {
        tar::Archive::new(flate2::read::GzDecoder::new(archive))
            .unpack(&part)
            .context("unpacking the virtx-uvm archive")?;
        anyhow::ensure!(
            part.join(server).is_file(),
            "the virtx-uvm archive has no {server}"
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

/// Rename `from` to `to`, over whatever is there. A file is replaced by the rename itself; a
/// directory cannot be, so an old one is removed first.
fn replace(from: &Path, to: &Path) -> anyhow::Result<()> {
    if from.is_dir() && to.is_dir() {
        std::fs::remove_dir_all(to).with_context(|| format!("removing {}", to.display()))?;
    }
    std::fs::rename(from, to)
        .with_context(|| format!("moving {} to {}", from.display(), to.display()))
}
