//! The connectors: what can be grafted into the workspace, and where.
//!
//! Each command here builds one [`FileSystem`] and hands it to [`WorkFs::mount`], which is the
//! whole of what "connecting Notion" means — there is no second notion of a connection kept
//! beside the tree, because the tree is where a connection is observable. The [`MountInfo`]
//! recorded alongside is a label, not a handle: it says which of these commands produced the
//! store at a path, since a `Box<dyn FileSystem>` cannot be asked.
//!
//! **A connection is confirmed before it is mounted.** Both network stores build offline —
//! `S3Fs::new` assembles a client and sends nothing, and `NotionFs::new` builds an HTTP client —
//! so a wrong key, region or bucket would otherwise be invisible until the tree was clicked, and
//! would arrive there as an `EIO` on a listing with nothing to say about which field was wrong.
//! Each command therefore makes exactly one request first and mounts only if it answers.

use std::path::{Path, PathBuf};

use cortex::fs::{FileSystem, NotionConfig, NotionFs, PassthroughFs, S3Config, S3Fs};
use serde::Deserialize;
use tauri::State;

use crate::{
    error::{Error, Result},
    state::{MountInfo, MountKind, Workspace},
};

/// What the S3 form collects. The same fields as [`S3Config`], as its own type so the window is
/// not typing against a struct it does not own.
#[derive(Deserialize)]
pub struct S3Form {
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub endpoint: Option<String>,
    pub key_prefix: Option<String>,
}

/// Everything currently mounted, root first.
#[tauri::command]
pub async fn mounts(state: State<'_, Workspace>) -> Result<Vec<MountInfo>> {
    Ok(state.mounts.read().await.clone())
}

/// A directory on this machine, served as part of the tree.
#[tauri::command]
pub async fn mount_local(
    state: State<'_, Workspace>,
    path: String,
    host_root: PathBuf,
) -> Result<MountInfo> {
    let meta = tokio::fs::metadata(&host_root).await?;
    if !meta.is_dir() {
        return Err(Error::msg("디렉터리를 선택해 주세요"));
    }

    let path = mount_path(&path)?;
    let info = MountInfo {
        path: path.clone(),
        kind: MountKind::Local,
        label: leaf(&path),
        detail: host_root.display().to_string(),
        // Read-write: `PassthroughFs` serves the host's own permissions, so what this promises
        // is that the store implements the write half — not that every file under it is
        // writable.
        writable: true,
    };
    attach(&state, info, PassthroughFs::new(host_root)).await
}

/// A Notion workspace, read-only, as `page.json` per page.
#[tauri::command]
pub async fn mount_notion(
    state: State<'_, Workspace>,
    path: String,
    api_key: String,
) -> Result<MountInfo> {
    let api_key = api_key.trim().to_string();
    if api_key.is_empty() {
        return Err(Error::msg("Notion 통합 토큰을 입력해 주세요"));
    }
    let store = NotionFs::new(&NotionConfig { api_key })?;
    // The confirming request. A listing of the root is what the tree would ask for first
    // anyway, so a token that cannot do it is a connection worth refusing now.
    store
        .list(Path::new(""))
        .await
        .map_err(|err| Error::msg(format!("Notion에 연결하지 못했습니다: {err}")))?;

    let path = mount_path(&path)?;
    let info = MountInfo {
        path: path.clone(),
        kind: MountKind::Notion,
        label: leaf(&path),
        detail: "Notion workspace · 읽기 전용".into(),
        writable: false,
    };
    attach(&state, info, store).await
}

/// An S3 bucket's keys, read-only, as a tree.
#[tauri::command]
pub async fn mount_s3(
    state: State<'_, Workspace>,
    path: String,
    form: S3Form,
) -> Result<MountInfo> {
    let config = S3Config {
        bucket: form.bucket.trim().to_string(),
        region: form.region.trim().to_string(),
        access_key_id: form.access_key_id.trim().to_string(),
        secret_access_key: form.secret_access_key.clone(),
        endpoint: non_empty(form.endpoint),
        key_prefix: non_empty(form.key_prefix),
    };
    if config.bucket.is_empty() {
        return Err(Error::msg("버킷 이름을 입력해 주세요"));
    }
    let detail = match &config.endpoint {
        Some(endpoint) => format!("s3://{} · {endpoint}", config.bucket),
        None => format!("s3://{} · {}", config.bucket, config.region),
    };

    let store = S3Fs::new(&config)?;
    store
        .check_reachable()
        .await
        .map_err(|err| Error::msg(format!("버킷에 연결하지 못했습니다: {err}")))?;

    let path = mount_path(&path)?;
    let info = MountInfo {
        path: path.clone(),
        kind: MountKind::S3,
        label: leaf(&path),
        detail,
        writable: false,
    };
    attach(&state, info, store).await
}

/// Detach the store at `path`.
///
/// Whatever it held goes with it — a `WorkFs` mount is the store, not a copy of one — which for
/// the two read-only connectors costs nothing and for a local directory means only that the
/// window stops looking at it.
#[tauri::command]
pub async fn unmount(state: State<'_, Workspace>, path: String) -> Result<()> {
    let path = mount_path(&path)?;
    state.fs.write().await.unmount(Path::new(&path))?;
    state.mounts.write().await.retain(|m| m.path != path);
    Ok(())
}

/// Record the mount, then make it — in that order.
///
/// The list is what refuses a duplicate path, so it has to be the thing that decides. Mounting
/// first and recording second would let a second store land in the tree before the refusal, and
/// the tree has no undo that leaves the first store where it was.
async fn attach<F: FileSystem + 'static>(
    state: &State<'_, Workspace>,
    info: MountInfo,
    store: F,
) -> Result<MountInfo> {
    state.remember_mount(info.clone()).await?;
    if let Err(err) = state.fs.write().await.mount(Path::new(&info.path), store) {
        state.mounts.write().await.retain(|m| m.path != info.path);
        return Err(err.into());
    }
    Ok(info)
}

/// A mount path as the sidebar spells it: `/`-rooted, no trailing slash, never the root itself.
///
/// The root is excluded because the session's own in-memory store is there and unmounting it
/// would leave the tree with nowhere to write — an empty workspace is the one thing a launch
/// guarantees, and a connector is not the thing that gets to take it away.
fn mount_path(path: &str) -> Result<String> {
    let cleaned = path.trim().trim_matches('/').trim();
    if cleaned.is_empty() {
        return Err(Error::msg("연결할 경로를 입력해 주세요 (예: /notion)"));
    }
    Ok(format!("/{cleaned}"))
}

/// The last component of a mount path, which is what the sidebar shows as its name.
fn leaf(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// `None` for a field the form left blank, which is what both optional `S3Config` fields mean.
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
