//! The HyperCLOVA tab's commands: the tree as one actor sees it, a file as that actor may read
//! it, and a run — started here, reported as `hcx` events on the window.
//!
//! Configuration is the environment the window was launched with: `CORTEX_HCX_WORKSPACE` (the
//! tree's root; defaults to the example workspace beside the agent crate), `CORTEX_HCX_S3`
//! (`bucket[/prefix]` serving `CORTEX_HCX_S3_AT`, default `재무팀`), `CORTEX_HCX_S3_REGION`,
//! `CORTEX_HCX_S3_ENDPOINT`, `CLOVASTUDIO_API_KEY`, `CLOVASTUDIO_OPENAI_URL`.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use cortex_agent_hyperclova::{
    session::{self, Config, Event, Node, Record, Summary},
    tools,
    tree::S3Source,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter as _, Manager as _, State};

use crate::error::{Error, Result};

pub const ACTORS: [&str; 3] = ["구매팀", "재무팀", "인사팀"];
pub const MODELS: [&str; 2] = ["HCX-007", "HCX-005"];

pub struct Hcx {
    pub workspace: PathBuf,
    pub s3: Option<(String, S3Source)>,
    running: AtomicBool,
}

impl Hcx {
    /// Where runs are written down: a hidden folder at the workspace root, which the tree
    /// listing skips like every other dot-entry.
    pub fn record_dir(&self) -> PathBuf {
        self.workspace.join(".runs")
    }

    pub fn from_env() -> Self {
        let workspace = std::env::var_os("CORTEX_HCX_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../cortex-agents/hyperclova/examples/procurement")
            });
        let s3 = std::env::var("CORTEX_HCX_S3")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|spec| {
                let at = std::env::var("CORTEX_HCX_S3_AT").unwrap_or_else(|_| "재무팀".into());
                let region = std::env::var("CORTEX_HCX_S3_REGION")
                    .unwrap_or_else(|_| "ap-northeast-2".into());
                let endpoint = std::env::var("CORTEX_HCX_S3_ENDPOINT")
                    .ok()
                    .filter(|v| !v.is_empty());
                (at, S3Source::parse(&spec, region, endpoint))
            });
        Hcx {
            workspace,
            s3,
            running: AtomicBool::new(false),
        }
    }
}

#[derive(Serialize)]
pub struct HcxConfig {
    pub workspace: String,
    pub actors: Vec<&'static str>,
    pub models: Vec<&'static str>,
    pub default_question: &'static str,
    pub s3: Option<String>,
    pub api_key_present: bool,
    pub mem_present: bool,
    /// `CORTEX_HCX_OPEN_LATEST=1`: the window opens on the most recent run rather than empty.
    pub open_latest: bool,
}

#[tauri::command]
pub async fn hcx_config(state: State<'_, Hcx>) -> Result<HcxConfig> {
    Ok(HcxConfig {
        workspace: state.workspace.display().to_string(),
        actors: ACTORS.to_vec(),
        models: MODELS.to_vec(),
        default_question: session::DEFAULT_QUESTION,
        s3: state.s3.as_ref().map(|(_, s)| s.describe()),
        api_key_present: std::env::var("CLOVASTUDIO_API_KEY").is_ok_and(|k| !k.is_empty()),
        mem_present: session::find_mem_bin(None).is_some(),
        open_latest: std::env::var("CORTEX_HCX_OPEN_LATEST").is_ok_and(|v| v == "1"),
    })
}

#[tauri::command]
pub async fn hcx_tree(state: State<'_, Hcx>, actor: String) -> Result<Vec<Node>> {
    let (fs, _) =
        session::open_tree(&state.workspace, &actor, state.s3.as_ref()).map_err(anyhow_err)?;
    session::tree_nodes(fs.as_ref()).await.map_err(anyhow_err)
}

#[tauri::command]
pub async fn hcx_read(state: State<'_, Hcx>, actor: String, path: String) -> Result<String> {
    let (fs, _) =
        session::open_tree(&state.workspace, &actor, state.s3.as_ref()).map_err(anyhow_err)?;
    let bytes = tools::read_all(fs.as_ref(), Path::new(&path)).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[tauri::command]
pub async fn hcx_runs(state: State<'_, Hcx>) -> Result<Vec<Summary>> {
    Ok(session::list_records(&state.record_dir()))
}

#[tauri::command]
pub async fn hcx_run_detail(state: State<'_, Hcx>, id: String) -> Result<Record> {
    session::load_record(&state.record_dir(), &id).map_err(anyhow_err)
}

#[tauri::command]
pub async fn hcx_run(
    app: AppHandle,
    state: State<'_, Hcx>,
    actor: String,
    model: String,
    question: String,
) -> Result<()> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err(Error::msg("이미 실행 중입니다"));
    }
    let api_key = std::env::var("CLOVASTUDIO_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            state.running.store(false, Ordering::SeqCst);
            Error::msg("CLOVASTUDIO_API_KEY 가 설정되지 않았습니다")
        })?;
    let cfg = Config {
        workspace: state.workspace.clone(),
        actor,
        model,
        question,
        s3: state.s3.clone(),
        api_key,
        url: std::env::var("CLOVASTUDIO_OPENAI_URL")
            .unwrap_or_else(|_| session::DEFAULT_URL.to_string()),
        mem_bin: None,
        reasoning_effort: None,
        record_dir: Some(state.record_dir()),
    };
    let emitter = app.clone();
    let sink: session::Sink = Arc::new(move |ev: Event| {
        let _ = emitter.emit("hcx", &ev);
    });
    // The run outlives this command: the caller gets `Ok` at once and the events afterwards.
    let running = Arc::new(RunningGuard(app.clone()));
    tauri::async_runtime::spawn(async move {
        let _guard = running;
        if let Err(err) = session::run(cfg, sink).await {
            let _ = app.emit(
                "hcx",
                &serde_json::json!({ "kind": "failed", "message": err.to_string() }),
            );
        }
    });
    Ok(())
}

/// Clears the running flag when the spawned run ends, however it ends.
struct RunningGuard(AppHandle);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if let Some(state) = self.0.try_state::<Hcx>() {
            state.running.store(false, Ordering::SeqCst);
        }
    }
}

fn anyhow_err(err: anyhow::Error) -> Error {
    Error::msg(format!("{err:#}"))
}
