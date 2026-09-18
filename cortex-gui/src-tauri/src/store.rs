//! Making a resource's store real — demo variant with no FUSE mount.
//!
//! Upstream mounts the workspace with FUSE-T and runs `mem init` / `index init` through
//! `cortex-local-console` against the mounted tree. This machine has no FUSE-T, so instead the
//! program is run against a temporary file on the host and the resulting bytes are written into
//! the workspace at the same path. The store that lands in the tree is byte-identical to what
//! the mounted route produces; only the route differs.

use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
};

use cortex::fs::WorkFs;
use tokio::sync::RwLock;

use crate::{
    error::{Error, Result},
    fsops::write_file,
    state::ResourceKind,
};

/// How long an `init` gets. It creates one small file.
const INIT_TIMEOUT_MS: u64 = 60_000;

/// Create the store for a resource, inside the workspace.
///
/// Returns whatever the program said on stdout, which is what the window shows.
pub async fn init_store(
    fs: Arc<RwLock<WorkFs>>,
    kind: ResourceKind,
    store_path: &str,
) -> Result<String> {
    let program = executable(kind)?;
    let scratch = tempdir()?;
    let file_name = Path::new(store_path)
        .file_name()
        .ok_or_else(|| Error::msg("저장소 경로에 파일 이름이 없습니다"))?
        .to_owned();
    let host_path = scratch.join(&file_name);

    let run = tokio::process::Command::new(&program)
        .arg("init")
        .arg(&host_path)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_millis(INIT_TIMEOUT_MS), run)
        .await
        .map_err(|_| {
            Error::msg(format!(
                "{} init 이 시간 안에 끝나지 않았습니다",
                program.display()
            ))
        })?
        .map_err(|err| {
            Error::msg(format!(
                "{} 를 실행하지 못했습니다: {err}",
                program.display()
            ))
        })?;

    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.trim();
        let _ = tokio::fs::remove_dir_all(&scratch).await;
        return Err(Error::msg(if said.is_empty() {
            format!(
                "{} init 이 {} 로 끝났습니다",
                program.display(),
                output.status
            )
        } else {
            said.to_string()
        }));
    }

    let bytes = tokio::fs::read(&host_path).await?;
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    {
        let guard = fs.read().await;
        write_file(&*guard, Path::new(store_path), &bytes).await?;
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A fresh directory for one `init`, under the system temp dir.
fn tempdir() -> Result<PathBuf> {
    let dir = env::temp_dir().join(format!(
        "cortex-gui-store-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The console server this window drives.
const CONSOLE_BIN: &str = "cortex-local-console";

/// The program that writes a store of this kind.
fn executable(kind: ResourceKind) -> Result<PathBuf> {
    let name = match kind {
        ResourceKind::Memory => "mem",
        ResourceKind::Docset => "index",
    };
    Ok(bin_dir()?.join(name))
}

/// The directory holding the executables this window shells out to.
///
/// `CORTEX_BIN_DIR` if it is set, otherwise the nearest `target/debug` or `target/release`
/// walking up from the working directory.
fn bin_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("CORTEX_BIN_DIR") {
        let dir = PathBuf::from(dir);
        return if complete(&dir) {
            Ok(dir)
        } else {
            Err(Error::msg(format!(
                "CORTEX_BIN_DIR={} 에 {CONSOLE_BIN}, mem, index 가 모두 있어야 합니다",
                dir.display()
            )))
        };
    }

    let mut here = env::current_dir()?;
    loop {
        for profile in ["debug", "release"] {
            let candidate = here.join("target").join(profile);
            if complete(&candidate) {
                return Ok(candidate);
            }
        }
        if !here.pop() {
            break;
        }
    }
    Err(Error::msg(format!(
        "{CONSOLE_BIN}, mem, index 를 찾지 못했습니다. \
         워크스페이스에서 `cargo build -p cortex-local-console -p cortex-exec-mem -p \
         cortex-exec-index` 를 실행하거나 CORTEX_BIN_DIR 을 지정해 주세요."
    )))
}

/// Whether `dir` holds every executable this module needs.
fn complete(dir: &Path) -> bool {
    [CONSOLE_BIN, "mem", "index"]
        .iter()
        .all(|name| dir.join(name).is_file())
}
