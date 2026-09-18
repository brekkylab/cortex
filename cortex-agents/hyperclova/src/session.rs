//! One run of the agent, reported as a sequence of [`Event`]s.
//!
//! Everything a front end shows — the mounts, each tool call and what the tree answered, the
//! audit trail, the closing check of the report against the denials — is an event handed to the
//! sink the caller passed in. The terminal binary prints them; a window renders them. Neither
//! knows anything the other does not.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use ailoy::{
    agent::{Agent, AgentProvider, AgentSpec, get_agent_providers_mut},
    lang_model::{LangModelProvider, get_lm_providers_mut},
    message::{Message, Part, Role},
    tool::get_tool_providers_mut,
};
use anyhow::Context as _;
use chrono::{DateTime, Local};
use cortex::fs::{DirentKind, FileSystem as _, WorkFs};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::{
    acl::{self, Acl, AclFs, readers_ko},
    audit::{Audit, Entry},
    tools::{self, Ctx, Mem},
    tree::{self, S3Source},
};

pub const DEFAULT_URL: &str = "https://clovastudio.stream.ntruss.com/v1/openai/chat/completions";
const PROVIDER: &str = "clovastudio";

pub const DEFAULT_QUESTION: &str = "다음 주 주간회의에 올릴 3분기 단가 협상 대상 선정안을 만들어 주세요. 협력사별 우선순위와 근거를 정리해 주세요.";

pub const INSTRUCTION: &str = "\
당신은 회사 내부 데이터 트리 위에서 일하는 업무 에이전트다. 트리는 부서 폴더(구매팀·재무팀·인사팀·회의록·정책)로 되어 있고, \
내용은 도구 ls·read·search 로만 확인할 수 있다. 읽지 않은 파일의 내용은 모른다고 간주한다.

작업 순서 — 반드시 이 순서로 도구를 호출한다:
1. recall 도구가 있으면 질문으로 recall 을 한 번 호출해 이전 결론을 확인한다.
2. 아래 '현재 트리' 에서 판단에 필요한 파일을 고른다. 폴더 안이 더 궁금하면 ls 로 확인한다.
3. 고른 파일을 read 로 전부 읽는다. 규정 파일(구매팀/구매규정-v7.md)은 항상 읽는다. \
   질문이 요구하는 자료(예: 여신한도, 납기 이력, 신용등급)가 🔒 표시여도 반드시 read 를 호출한다 — 거절 응답(permission_denied) 자체가 보고서의 근거다. \
   시도하지 않은 자료를 '권한 밖' 이나 '확인하지 못한 자료 없음' 으로 적는 것은 금지다.
4. 여러 파일을 대조해 결론을 낸다. 모든 수치·사실 옆에 근거 파일 경로를 적고, 규정은 조항 번호(예: 3.1)로 인용한다.
5. permission_denied 가 온 자료는 우회하지 않는다. 보고서에 '권한 밖 — ○○팀 전용' 으로 적고, 확인 가능한 자료만으로 결론을 낸다. \
   읽지 못한 자료의 수치·이름을 지어내는 것은 금지다. 결론에 꼭 필요한 자료가 권한 밖이면, 보고서 본문을 '작성 불가' 로 하고 필요한 자료·담당 부서·요청 방법만 적는다.
6. write_report 로 산출물/ 아래에 마크다운 보고서를 저장한다. sources 에는 read 한 파일 경로를 전부 넣는다. 거절되면 사유를 읽고 고쳐서 다시 호출한다.
7. remember 도구가 있으면 핵심 결론 한 문장을 남긴다.

문체: 한국어, 표와 불릿, 결론 먼저. 표 안에서는 셀마다 근거 경로를 짧게 붙인다. \
파일은 트리 경로(예: 구매팀/구매규정-v7.md)로만 가리키고 URL 이나 링크는 만들지 않는다. \
마지막 답변은 세 줄로: 보고서 저장 위치, 산출물의 열람 권한, 권한 밖이라 확인하지 못한 자료.";

const SAVE_NUDGE: &str = "보고서가 아직 저장되지 않았다. 지금 write_report 를 호출해 산출물/ 아래에 저장한다. \
sources 에는 이 실행에서 read 로 읽은 파일만 넣는다. 읽은 파일이 없으면 본문을 '작성 불가' 로 하고 sources 에는 읽은 정책·규정 파일을 넣는다.";

/// Everything a run is given.
#[derive(Clone, Debug)]
pub struct Config {
    pub workspace: PathBuf,
    pub actor: String,
    pub model: String,
    pub question: String,
    /// `(top-level directory, bucket)`: that directory is served from the bucket.
    pub s3: Option<(String, S3Source)>,
    pub api_key: String,
    pub url: String,
    pub mem_bin: Option<PathBuf>,
    /// `None` picks the model's default: `"none"` on HCX-007, nothing elsewhere.
    pub reasoning_effort: Option<String>,
    /// Where the run is written down as a [`Record`] when it ends; `None` keeps nothing.
    pub record_dir: Option<PathBuf>,
}

impl Config {
    /// CLOVA Studio accepts Function Calling on `HCX-007` only with reasoning switched off; on
    /// the OpenAI-compatible endpoint that is the standard `reasoning_effort` field, `"none"`.
    pub fn effective_reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort.clone().or_else(|| {
            self.model
                .eq_ignore_ascii_case("HCX-007")
                .then(|| "none".to_string())
        })
    }
}

/// One function the model asked for.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    pub name: String,
    pub arguments: serde_json::Value,
}

/// What a run reports, in order.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Started {
        actor: String,
        model: String,
        question: String,
        mounts: Vec<Mount>,
    },
    /// The model spoke: text, tool calls, or both.
    Assistant {
        text: Option<String>,
        calls: Vec<Call>,
    },
    /// The tree answered one call.
    ToolResult {
        value: serde_json::Value,
        denied: bool,
    },
    /// Something the harness did on its own, said in one line.
    Notice {
        text: String,
    },
    Audit {
        entry: Entry,
    },
    /// The closing check: every source the tree refused, and whether the report names it.
    Check {
        report: Option<String>,
        denied: Vec<Denied>,
    },
    Finished {
        seconds: f64,
        log: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mount {
    pub name: String,
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Denied {
    pub path: String,
    pub mentioned: bool,
}

/// One entry of the tree as one actor sees it.
#[derive(Clone, Debug, Serialize)]
pub struct Node {
    pub path: String,
    pub name: String,
    pub kind: &'static str,
    pub depth: usize,
    /// `open`, `locked` (the policy closes it), or `inherited` (a written file's sidecar does).
    pub access: &'static str,
    pub label: String,
    pub readers: String,
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// One run, written down whole: what was asked, by whom, of which model, and every event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub actor: String,
    pub model: String,
    pub question: String,
    pub started: DateTime<Local>,
    pub finished: Option<DateTime<Local>>,
    /// `false` when the run ended in an error; the last event then says which.
    pub ok: bool,
    pub events: Vec<Event>,
}

/// A run as a list shows it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    pub actor: String,
    pub model: String,
    pub question: String,
    pub started: DateTime<Local>,
    pub finished: Option<DateTime<Local>>,
    pub ok: bool,
    /// The report the run wrote, if it wrote one.
    pub report: Option<String>,
    pub seconds: Option<f64>,
}

impl Record {
    pub fn summary(&self) -> Summary {
        let mut report = None;
        let mut seconds = None;
        for e in &self.events {
            match e {
                Event::Check { report: r, .. } => report = r.clone(),
                Event::Finished { seconds: s, .. } => seconds = Some(*s),
                _ => {}
            }
        }
        Summary {
            id: self.id.clone(),
            actor: self.actor.clone(),
            model: self.model.clone(),
            question: self.question.clone(),
            started: self.started,
            finished: self.finished,
            ok: self.ok,
            report,
            seconds,
        }
    }
}

/// Every record under `dir`, newest first.
pub fn list_records(dir: &Path) -> Vec<Summary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Summary> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice::<Record>(&b).ok())
        .map(|r| r.summary())
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.started));
    out
}

pub fn load_record(dir: &Path, id: &str) -> anyhow::Result<Record> {
    anyhow::ensure!(
        !id.is_empty() && id.chars().all(|c| c.is_alphanumeric() || c == '-'),
        "not a record id"
    );
    let bytes = std::fs::read(dir.join(format!("{id}.json")))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Build the tree for one actor.
pub fn open_tree(
    workspace: &Path,
    actor: &str,
    s3: Option<&(String, S3Source)>,
) -> anyhow::Result<(Arc<AclFs<WorkFs>>, Vec<Mount>)> {
    let acl_text = std::fs::read_to_string(workspace.join("정책/acl.json"))
        .with_context(|| format!("reading {}/정책/acl.json", workspace.display()))?;
    let acl = Arc::new(Acl::from_json(&acl_text)?);
    let (workfs, mounted) = tree::build(workspace, s3.map(|(at, src)| (at.as_str(), src)))?;
    let mounts = mounted
        .into_iter()
        .map(|m| Mount {
            name: m.name,
            source: m.source,
        })
        .collect();
    Ok((Arc::new(AclFs::new(workfs, acl, actor.to_string())), mounts))
}

/// The whole tree, depth first, with the actor's access to every entry.
pub async fn tree_nodes(fs: &AclFs<WorkFs>) -> anyhow::Result<Vec<Node>> {
    let mut out = Vec::new();
    walk_nodes(fs, PathBuf::new(), 0, &mut out).await?;
    Ok(out)
}

fn walk_nodes<'a>(
    fs: &'a AclFs<WorkFs>,
    dir: PathBuf,
    depth: usize,
    out: &'a mut Vec<Node>,
) -> futures::future::BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
        let mut entries = match fs.list(&dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        entries.sort_by(|a, b| {
            (a.kind == DirentKind::File, &a.name).cmp(&(b.kind == DirentKind::File, &b.name))
        });
        for e in entries {
            if e.name.starts_with('.') {
                continue;
            }
            let p = dir.join(&e.name);
            let v = fs.verdict(&p);
            let access = match e.kind {
                DirentKind::Dir if !v.readable => "locked",
                DirentKind::Dir => "open",
                DirentKind::File if !v.readable => "locked",
                DirentKind::File if !fs.may_read(&p).await => "inherited",
                DirentKind::File => "open",
            };
            out.push(Node {
                path: p.display().to_string(),
                name: e.name.clone(),
                kind: match e.kind {
                    DirentKind::Dir => "dir",
                    DirentKind::File => "file",
                },
                depth,
                access,
                label: v.label.clone(),
                readers: readers_ko(&v.readers),
            });
            if e.kind == DirentKind::Dir && v.readable {
                walk_nodes(fs, p, depth + 1, out).await?;
            }
        }
        Ok(())
    })
}

/// The tree as text for the system prompt: one line per file with the actor's access.
async fn tree_snapshot(fs: &AclFs<WorkFs>) -> anyhow::Result<String> {
    let mut out = String::new();
    for n in tree_nodes(fs).await? {
        if n.kind == "dir" {
            if n.access == "locked" {
                out.push_str(&format!(
                    "- {}/  [🔒 폴더 전체가 권한 밖 — {} 전용 · 안에 무엇이 있는지도 보이지 않음]\n",
                    n.path, n.readers
                ));
            }
            continue;
        }
        let access = match n.access {
            "open" => "열람 가능".to_string(),
            "inherited" => "🔒 권한 밖 — 인용 자료의 권한을 따름".to_string(),
            _ => format!(
                "🔒 권한 밖 — {} 전용 · 질문과 관련되면 read 를 시도해 거절 사유를 받을 것",
                n.readers
            ),
        };
        out.push_str(&format!("- {}  [{}]\n", n.path, access));
    }
    Ok(out)
}

/// Where `mem` lives: `--mem-bin`, or `target/debug/mem` beside this workspace's build output.
pub fn find_mem_bin(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(b) = explicit {
        return Some(b.to_path_buf());
    }
    let guess = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mem");
    guess.exists().then_some(guess)
}

async fn resolve_mem(cfg: &Config) -> anyhow::Result<Option<Mem>> {
    let Some(bin) = find_mem_bin(cfg.mem_bin.as_deref()) else {
        return Ok(None);
    };
    // One store per actor: what a department's agent concluded from files only it may read is
    // itself something only that department may recall.
    let dir = tree::output_dir(&cfg.workspace)?.join(&cfg.actor);
    std::fs::create_dir_all(&dir)?;
    let store = dir.join(".memory.sqlite");
    if !store.exists() {
        let out = tokio::process::Command::new(&bin)
            .args(["init", &store.display().to_string()])
            .output()
            .await
            .with_context(|| format!("running {}", bin.display()))?;
        anyhow::ensure!(
            out.status.success(),
            "mem init: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let label = Path::new("산출물").join(&cfg.actor).join(".memory.sqlite");
    Ok(Some(Mem { bin, store, label }))
}

/// Run the agent once, reporting to `sink` as it goes, and writing the run down afterwards
/// when `cfg.record_dir` says where.
pub async fn run(cfg: Config, sink: Sink) -> anyhow::Result<()> {
    let Some(dir) = cfg.record_dir.clone() else {
        return run_inner(cfg, sink).await;
    };
    let started_at = Local::now();
    let id = format!(
        "{}-{}",
        started_at.format("%Y%m%d-%H%M%S"),
        cfg.actor_slug()
    );
    let events: Arc<std::sync::Mutex<Vec<Event>>> = Default::default();
    let recording = {
        let events = events.clone();
        Arc::new(move |ev: Event| {
            events.lock().unwrap().push(ev.clone());
            sink(ev);
        }) as Sink
    };
    let outcome = run_inner(cfg.clone(), recording).await;
    let record = Record {
        id,
        actor: cfg.actor.clone(),
        model: cfg.model.clone(),
        question: cfg.question.clone(),
        started: started_at,
        finished: Some(Local::now()),
        ok: outcome.is_ok(),
        events: std::mem::take(&mut *events.lock().unwrap()),
    };
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join(format!("{}.json", record.id)),
        serde_json::to_vec_pretty(&record)?,
    )?;
    outcome
}

impl Config {
    /// The actor as a file name can carry it: letters stay, separators do not.
    fn actor_slug(&self) -> String {
        self.actor
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect()
    }
}

async fn run_inner(cfg: Config, sink: Sink) -> anyhow::Result<()> {
    let started = Instant::now();
    let (fs, mounts) = open_tree(&cfg.workspace, &cfg.actor, cfg.s3.as_ref())?;
    sink(Event::Started {
        actor: cfg.actor.clone(),
        model: cfg.model.clone(),
        question: cfg.question.clone(),
        mounts,
    });

    let mem = resolve_mem(&cfg).await?;
    let audit = Audit::default();
    let ctx = Arc::new(Ctx {
        fs: fs.clone(),
        audit: audit.clone(),
        model: cfg.model.clone(),
        mem,
        seen: Default::default(),
    });

    // Three registries, one name: the model endpoint, the tools, and the pairing of the two.
    {
        let mut lmp = LangModelProvider::new();
        lmp.insert(
            "hyperclova/*".into(),
            LangModelProvider::chat_completion(&cfg.url, Some(cfg.api_key.clone()))?,
        );
        get_lm_providers_mut().insert(PROVIDER.into(), lmp);
        get_tool_providers_mut().insert(PROVIDER.into(), tools::provider(ctx.clone()));
        get_agent_providers_mut().insert(PROVIDER.into(), AgentProvider::new(PROVIDER, PROVIDER));
    }

    let instruction = format!(
        "{INSTRUCTION}\n\n현재 트리 — {} 기준 열람 가능 여부:\n{}",
        cfg.actor,
        tree_snapshot(fs.as_ref()).await?
    );
    let mut spec = AgentSpec::new(format!("hyperclova/{}", cfg.model))
        .instruction(instruction)
        .tools(tools::descs(ctx.mem.is_some()));
    if let Some(effort) = cfg.effective_reasoning_effort() {
        spec = spec.reasoning_effort(effort);
    }
    let mut agent = Agent::try_with_provider(spec, PROVIDER)?;

    // Audit entries reach the sink as they are recorded, interleaved with the messages.
    let mut reported = 0usize;
    let mut turn = async |agent: &mut Agent, text: &str| -> anyhow::Result<()> {
        let mut stream =
            agent.run(Message::new(Role::User).with_contents([Part::text(text.to_string())]));
        while let Some(out) = stream.next().await {
            let out = out?;
            for e in audit.entries().into_iter().skip(reported) {
                sink(Event::Audit { entry: e });
                reported += 1;
            }
            if let Some(ev) = message_event(&out.message) {
                sink(ev);
            }
        }
        Ok(())
    };
    turn(&mut agent, &cfg.question).await?;
    // A turn that ends without a saved report is not finished; the audit log, not the model's
    // account of itself, says which it was.
    if !audit
        .entries()
        .iter()
        .any(|e| e.tool == "write_report" && e.allowed)
    {
        sink(Event::Notice {
            text: "write_report 가 호출되지 않았다 — 저장을 요청한다".into(),
        });
        turn(&mut agent, SAVE_NUDGE).await?;
    }
    for e in audit.entries().into_iter().skip(reported) {
        sink(Event::Audit { entry: e });
    }

    sink(check(fs.as_ref(), &audit).await);

    let log_path = Path::new("산출물").join(&cfg.actor).join(format!(
        "감사로그-{}.jsonl",
        Local::now().format("%Y%m%d-%H%M%S")
    ));
    tools::write_all(fs.as_ref(), &log_path, audit.to_jsonl().as_bytes()).await?;
    // The log names files the actor was refused; that is the actor's business and its
    // compliance owner's, not every department's.
    let side = serde_json::json!({ "readers": [cfg.actor], "author": cfg.actor, "model": cfg.model,
        "created": Local::now().to_rfc3339() });
    tools::write_all(
        fs.as_ref(),
        &acl::sidecar_of(&log_path),
        serde_json::to_string_pretty(&side)?.as_bytes(),
    )
    .await?;
    sink(Event::Finished {
        seconds: started.elapsed().as_secs_f64(),
        log: log_path.display().to_string(),
    });
    Ok(())
}

fn message_event(msg: &Message) -> Option<Event> {
    match msg.role {
        Role::Assistant => {
            let calls: Vec<Call> = msg
                .tool_calls
                .iter()
                .flatten()
                .filter_map(|c| match c {
                    Part::Function { function, .. } => Some(Call {
                        name: function.name.clone(),
                        arguments: serde_json::to_value(&function.arguments).unwrap_or_default(),
                    }),
                    _ => None,
                })
                .collect();
            let text: String = msg
                .contents
                .iter()
                .filter_map(|p| match p {
                    Part::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = (!text.trim().is_empty()).then_some(text);
            if text.is_none() && calls.is_empty() {
                return None;
            }
            Some(Event::Assistant { text, calls })
        }
        Role::Tool => {
            let value = msg
                .contents
                .iter()
                .find_map(|p| match p {
                    Part::Value { value } => serde_json::to_value(value).ok(),
                    Part::Text { text } => Some(serde_json::Value::String(text.clone())),
                    _ => None,
                })
                .unwrap_or(serde_json::Value::Null);
            let denied = value
                .get("error")
                .and_then(|e| e.as_str())
                .is_some_and(|e| e == "permission_denied");
            Some(Event::ToolResult { value, denied })
        }
        _ => None,
    }
}

/// The model's closing summary is its own account; this is the tree's. Every source the tree
/// refused is looked for, by path, in the report that was written.
async fn check(fs: &AclFs<WorkFs>, audit: &Audit) -> Event {
    let entries = audit.entries();
    let report = entries
        .iter()
        .rev()
        .find(|e| e.tool == "write_report" && e.allowed)
        .map(|e| PathBuf::from(&e.path));
    let body = match &report {
        Some(r) => tools::read_all(fs, r)
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default(),
        None => String::new(),
    };
    let mut paths: Vec<&str> = entries
        .iter()
        .filter(|e| e.tool == "read" && !e.allowed && e.detail.contains("닫혀 있음"))
        .map(|e| e.path.as_str())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    Event::Check {
        report: report.map(|r| r.display().to_string()),
        denied: paths
            .into_iter()
            .map(|p| Denied {
                path: p.to_string(),
                mentioned: body.contains(p),
            })
            .collect(),
    }
}
