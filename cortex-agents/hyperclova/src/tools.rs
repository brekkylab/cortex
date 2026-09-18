//! What HyperCLOVA X can do to the tree, as CLOVA Studio Function Calling tools.
//!
//! Four tools over the filesystem — `ls`, `read`, `search`, `write_report` — and two over
//! cortex's `mem` executable — `remember`, `recall`. Every one goes through the [`AclFs`] the
//! run was built for, so a denial reaches the model as a tool result it can report rather than
//! as bytes it should not have seen. The schemas here are what `AgentSpec::tools` carries and
//! what ailoy marshals into the `tools` array of the OpenAI-compatible request.

use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use ailoy::{
    tool::{ToolDesc, ToolDescBuilder, ToolFunc, ToolProvider},
    tool_func,
};
use chrono::Local;
use cortex::fs::{DirentKind, FileSystem, WorkFs};
use serde_json::json;

use crate::{
    acl::{AclFs, normalize, readers_ko, sidecar_of},
    audit::Audit,
};

pub use crate::acl::read_whole as read_all;

/// Reads longer than this are cut, with a note; the model's context is not a pager.
const READ_CAP: usize = 12_000;
/// Hits a `search` hands back at most.
const SEARCH_CAP: usize = 40;

/// Everything a tool call needs, shared by all of them for the length of a run.
pub struct Ctx {
    pub fs: Arc<AclFs<WorkFs>>,
    pub audit: Audit,
    pub model: String,
    /// `mem`, when the workspace was built with it; `None` leaves the memory tools out.
    pub mem: Option<Mem>,
    /// Files whose contents reached the model in this run — by `read`, or as `search` hits.
    /// A report may cite only these: provenance is what was actually opened, not what the
    /// model says it consulted.
    pub seen: Mutex<BTreeSet<PathBuf>>,
}

pub struct Mem {
    pub bin: PathBuf,
    /// Where `mem` finds the store: a host path, since `mem` is a program and not a tool.
    pub store: PathBuf,
    /// The same store as the tree spells it, which is what the audit log names.
    pub label: PathBuf,
}

impl Ctx {
    fn actor(&self) -> &str {
        self.fs.actor()
    }

    fn saw(&self, path: &Path) {
        self.seen.lock().unwrap().insert(path.to_path_buf());
    }

    fn has_seen(&self, path: &Path) -> bool {
        self.seen.lock().unwrap().contains(path)
    }
}

/// Replace a file's contents: create if new, truncate, write.
pub async fn write_all(fs: &dyn FileSystem, path: &Path, bytes: &[u8]) -> io::Result<()> {
    match fs.create(path).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => fs.truncate(path, 0).await?,
        Err(e) => return Err(e),
    }
    let mut off = 0usize;
    while off < bytes.len() {
        let n = fs.write_at(path, &bytes[off..], off as u64).await?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        off += n;
    }
    fs.flush(path).await
}

/// Every file under `dir`, depth first, dot-files skipped.
pub async fn walk(fs: &dyn FileSystem, dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        // A folder the actor may not open holds nothing the actor can be shown.
        let Ok(mut entries) = fs.list(&d).await else {
            continue;
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for e in entries {
            if e.name.starts_with('.') {
                continue;
            }
            let p = d.join(&e.name);
            match e.kind {
                DirentKind::Dir => stack.push(p),
                DirentKind::File => out.push(p),
            }
        }
    }
    out.sort();
    Ok(out)
}

fn arg_str<'a>(args: &'a ailoy::datatype::Value, key: &str) -> &'a str {
    args.pointer(&format!("/{key}"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

fn arg_strs(args: &ailoy::datatype::Value, key: &str) -> Vec<String> {
    args.pointer(&format!("/{key}"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn denied(e: &io::Error) -> serde_json::Value {
    json!({ "error": "permission_denied", "message": e.to_string(),
            "hint": "이 자료는 현재 사용자에게 닫혀 있다. 우회하지 말고 보고서에 '권한 밖'으로 표시한다." })
}

async fn access_ko(ctx: &Ctx, path: &Path) -> String {
    let v = ctx.fs.verdict(path);
    if !v.readable {
        return format!("🔒 {} — {} 전용", v.label, readers_ko(&v.readers));
    }
    if !ctx.fs.may_read(path).await {
        return format!("🔒 {} — 인용 자료의 권한에 따라 닫혀 있음", v.label);
    }
    format!("열람 가능 ({})", v.label)
}

pub fn descs(with_mem: bool) -> Vec<ToolDesc> {
    let mut v = vec![
        ToolDescBuilder::new("ls")
            .description("조직 트리의 한 디렉터리를 나열한다. 루트는 \"\" 또는 \"/\". 각 항목에 현재 사용자의 열람 가능 여부가 붙어 온다.")
            .parameters(json!({"type":"object","properties":{"path":{"type":"string","description":"루트 기준 경로, 예: 구매팀/협력사평가"}},"required":["path"]}))
            .build(),
        ToolDescBuilder::new("read")
            .description("파일 하나의 내용을 읽는다. 열람 권한이 없으면 permission_denied 가 온다.")
            .parameters(json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}))
            .build(),
        ToolDescBuilder::new("search")
            .description("트리 전체(또는 path 아래)에서 문자열을 찾는다. 열람 가능한 파일만 검색되고, 권한 밖 파일은 개수와 경로만 locked 로 온다.")
            .parameters(json!({"type":"object","properties":{"query":{"type":"string"},"path":{"type":"string","description":"생략하면 루트"}},"required":["query"]}))
            .build(),
        ToolDescBuilder::new("write_report")
            .description("보고서를 산출물/ 아래에 저장한다. sources 에는 이 실행에서 read 로 읽은 원본 경로만 적을 수 있고(읽지 않은 파일은 거절), 산출물의 열람 권한은 인용 자료 중 가장 좁은 권한으로 정해진다.")
            .parameters(json!({"type":"object","properties":{
                "path":{"type":"string","description":"파일명, 예: 위험거래처-리포트-2026-09-22.md. 산출물/<사용자>/ 아래에 저장된다."},
                "content":{"type":"string","description":"마크다운 본문. 각 주장 옆에 근거 파일 경로를 적는다."},
                "sources":{"type":"array","items":{"type":"string"},"description":"인용한 원본 경로 목록"}},
                "required":["path","content","sources"]}))
            .build(),
    ];
    if with_mem {
        v.push(
            ToolDescBuilder::new("remember")
                .description("이번 작업에서 얻은 결론 한 문장을 에이전트 메모리(cortex mem)에 기록한다. 다음 실행의 recall 로 꺼낼 수 있다.")
                .parameters(json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}))
                .build(),
        );
        v.push(
            ToolDescBuilder::new("recall")
                .description("에이전트 메모리(cortex mem)에서 질의에 가까운 기록을 찾는다. 작업을 시작할 때 한 번 호출해 이전 결론을 확인한다.")
                .parameters(json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}))
                .build(),
        );
    }
    v
}

pub fn provider(ctx: Arc<Ctx>) -> ToolProvider {
    let mut tp = ToolProvider::empty();
    tp.insert_func("ls", ls(ctx.clone()));
    tp.insert_func("read", read(ctx.clone()));
    tp.insert_func("search", search(ctx.clone()));
    tp.insert_func("write_report", write_report(ctx.clone()));
    if ctx.mem.is_some() {
        tp.insert_func("remember", remember(ctx.clone()));
        tp.insert_func("recall", recall(ctx));
    }
    tp
}

fn ls(ctx: Arc<Ctx>) -> ToolFunc {
    tool_func!(async |args: Value| -> Value with [ctx = ctx.clone()] {
        let path = normalize(Path::new(arg_str(&args, "path")));
        let out = match ctx.fs.list(&path).await {
            Ok(mut entries) => {
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                let mut items = Vec::new();
                for e in entries.iter().filter(|e| !e.name.starts_with('.')) {
                    let p = path.join(&e.name);
                    items.push(json!({
                        "name": e.name,
                        "kind": match e.kind { DirentKind::Dir => "dir", DirentKind::File => "file" },
                        "access": access_ko(&ctx, &p).await,
                    }));
                }
                ctx.audit.record(ctx.actor(), "ls", &path, true, format!("{} entries", items.len()));
                json!({ "path": path.display().to_string(), "entries": items })
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                ctx.audit.record(ctx.actor(), "ls", &path, false, e.to_string());
                denied(&e)
            }
            Err(e) => {
                ctx.audit.record(ctx.actor(), "ls", &path, false, e.to_string());
                json!({ "error": e.kind().to_string(), "message": e.to_string() })
            }
        };
        out.into()
    })
}

fn read(ctx: Arc<Ctx>) -> ToolFunc {
    tool_func!(async |args: Value| -> Value with [ctx = ctx.clone()] {
        let path = normalize(Path::new(arg_str(&args, "path")));
        let out = match read_all(ctx.fs.as_ref(), &path).await {
            Ok(bytes) => {
                let mut text = String::from_utf8_lossy(&bytes).into_owned();
                let total = text.chars().count();
                if total > READ_CAP {
                    text = text.chars().take(READ_CAP).collect();
                    text.push_str(&format!("\n… (총 {total}자 중 {READ_CAP}자까지)"));
                }
                ctx.saw(&path);
                ctx.audit.record(ctx.actor(), "read", &path, true, format!("{} bytes", bytes.len()));
                json!({ "path": path.display().to_string(), "content": text })
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                ctx.audit.record(ctx.actor(), "read", &path, false, e.to_string());
                denied(&e)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // A path spelled from memory is usually a real file under a folder the caller
                // left off. Name the candidates, with what the caller may do with each.
                let candidates = same_name(&ctx, &path).await;
                ctx.audit.record(ctx.actor(), "read", &path, false, format!("not found; {} candidates", candidates.len()));
                json!({ "error": "not_found", "message": format!("{} 은(는) 없다", path.display()), "did_you_mean": candidates })
            }
            Err(e) => {
                ctx.audit.record(ctx.actor(), "read", &path, false, e.to_string());
                json!({ "error": e.kind().to_string(), "message": e.to_string() })
            }
        };
        out.into()
    })
}

/// Files anywhere in the tree with the same final component as `path`, each with the caller's
/// access to it.
async fn same_name(ctx: &Ctx, path: &Path) -> Vec<serde_json::Value> {
    let Some(name) = path.file_name() else {
        return Vec::new();
    };
    let files = walk(ctx.fs.as_ref(), Path::new(""))
        .await
        .unwrap_or_default();
    let mut out = Vec::new();
    for f in files.into_iter().filter(|f| f.file_name() == Some(name)) {
        out.push(json!({ "path": f.display().to_string(), "access": access_ko(ctx, &f).await }));
    }
    out
}

fn search(ctx: Arc<Ctx>) -> ToolFunc {
    tool_func!(async |args: Value| -> Value with [ctx = ctx.clone()] {
        let query = arg_str(&args, "query").to_string();
        let root = normalize(Path::new(arg_str(&args, "path")));
        let needle = query.to_lowercase();
        let mut hits = Vec::new();
        let mut locked = Vec::new();
        let mut scanned = 0usize;
        let out = match walk(ctx.fs.as_ref(), &root).await {
            Ok(files) => {
                for f in files {
                    if !ctx.fs.may_read(&f).await {
                        let v = ctx.fs.verdict(&f);
                        locked.push(json!({ "path": f.display().to_string(), "readers": readers_ko(&v.readers), "label": v.label }));
                        continue;
                    }
                    scanned += 1;
                    if let Ok(bytes) = read_all(ctx.fs.as_ref(), &f).await {
                        let text = String::from_utf8_lossy(&bytes);
                        for (i, line) in text.lines().enumerate() {
                            if line.to_lowercase().contains(&needle) {
                                ctx.saw(&f);
                                hits.push(json!({ "path": f.display().to_string(), "line": i + 1, "text": line.trim() }));
                                if hits.len() >= SEARCH_CAP { break; }
                            }
                        }
                    }
                    if hits.len() >= SEARCH_CAP { break; }
                }
                ctx.audit.record(ctx.actor(), "search", &root, true,
                    format!("q={query:?} hits={} scanned={scanned} locked={}", hits.len(), locked.len()));
                json!({ "query": query, "hits": hits, "scanned_files": scanned, "locked": locked })
            }
            Err(e) => {
                ctx.audit.record(ctx.actor(), "search", &root, false, e.to_string());
                json!({ "error": e.kind().to_string(), "message": e.to_string() })
            }
        };
        out.into()
    })
}

fn write_report(ctx: Arc<Ctx>) -> ToolFunc {
    tool_func!(async |args: Value| -> Value with [ctx = ctx.clone()] {
        // Every actor writes into its own folder under 산출물/, whatever the model spelled: two
        // departments asking the same question on the same day must not overwrite each other.
        let given = normalize(Path::new(arg_str(&args, "path")));
        let name = given.file_name().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("보고서.md"));
        let dir = Path::new("산출물").join(ctx.actor());
        let path = dir.join(name);
        let content = arg_str(&args, "content").to_string();
        // What the report cites: the `sources` list, plus every tree path the body mentions.
        let mut sources: BTreeSet<PathBuf> = arg_strs(&args, "sources").iter().map(|s| normalize(Path::new(s))).collect();
        for p in ctx.seen.lock().unwrap().iter() {
            if content.contains(&p.display().to_string()) {
                sources.insert(p.clone());
            }
        }
        let sources: Vec<PathBuf> = sources.into_iter().collect();

        // A report rests on files that were opened in this run, and on nothing else.
        if sources.is_empty() {
            let msg = "인용할 원본이 없다. 먼저 read 로 파일을 읽고, 그 경로를 sources 에 적는다".to_string();
            ctx.audit.record(ctx.actor(), "write_report", &path, false, msg.clone());
            return json!({ "error": "no_sources", "message": msg }).into();
        }
        if let Some(unread) = sources.iter().find(|s| !ctx.has_seen(s)) {
            let msg = format!("{} 은(는) 이 실행에서 읽지 않았다. read 로 확인한 뒤 인용한다", unread.display());
            ctx.audit.record(ctx.actor(), "write_report", &path, false, msg.clone());
            return json!({ "error": "unread_source", "message": msg }).into();
        }
        // …and only on files its author could open.
        if let Some(bad) = sources.iter().find(|s| !ctx.fs.verdict(s).readable) {
            let v = ctx.fs.verdict(bad);
            let msg = format!("{} 은(는) {} 전용이라 {} 이(가) 인용할 수 없다", bad.display(), readers_ko(&v.readers), ctx.actor());
            ctx.audit.record(ctx.actor(), "write_report", &path, false, msg.clone());
            return json!({ "error": "permission_denied", "message": msg }).into();
        }

        let readers: Vec<String> = ctx.fs.acl().derive(&sources).into_iter().collect();
        let now = Local::now();
        let footer = format!(
            "\n\n---\n작성: {} 담당 에이전트 · 모델 {} · {}\n열람 권한: {} (인용 자료 기준 최소 권한)\n인용: {}\n",
            ctx.actor(), ctx.model, now.format("%Y-%m-%d %H:%M"),
            readers_ko(&readers),
            sources.iter().map(|s| s.display().to_string()).collect::<Vec<_>>().join(", "),
        );
        let body = format!("{}{}", content.trim_end(), footer);
        let sidecar = sidecar_of(&path);
        let meta = json!({
            "readers": readers,
            "sources": sources.iter().map(|s| s.display().to_string()).collect::<Vec<_>>(),
            "author": ctx.actor(),
            "model": ctx.model,
            "created": now.to_rfc3339(),
        });
        match ctx.fs.mkdir(&dir).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                ctx.audit.record(ctx.actor(), "write_report", &dir, false, e.to_string());
                return json!({ "error": e.kind().to_string(), "message": e.to_string() }).into();
            }
        }
        let out = match write_all(ctx.fs.as_ref(), &path, body.as_bytes()).await {
            Ok(()) => {
                let _ = write_all(ctx.fs.as_ref(), &sidecar, serde_json::to_string_pretty(&meta).unwrap().as_bytes()).await;
                ctx.audit.record(ctx.actor(), "write_report", &path, true,
                    format!("{} bytes, readers={}", body.len(), readers_ko(&readers)));
                json!({ "written": path.display().to_string(), "acl": sidecar.display().to_string(),
                        "readers": readers_ko(&readers), "sources": sources.len() })
            }
            Err(e) => {
                ctx.audit.record(ctx.actor(), "write_report", &path, false, e.to_string());
                json!({ "error": e.kind().to_string(), "message": e.to_string() })
            }
        };
        out.into()
    })
}

async fn mem_run(mem: &Mem, argv: &[&str]) -> io::Result<String> {
    let out = tokio::process::Command::new(&mem.bin)
        .args(argv)
        .output()
        .await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

fn remember(ctx: Arc<Ctx>) -> ToolFunc {
    tool_func!(async |args: Value| -> Value with [ctx = ctx.clone()] {
        let text = format!("[{} · {}] {}", ctx.actor(), Local::now().format("%Y-%m-%d"), arg_str(&args, "text"));
        let mem = ctx.mem.as_ref().expect("remember is registered only with mem");
        let store = mem.store.display().to_string();
        let out = match mem_run(mem, &["insert", &store, &text]).await {
            Ok(_) => { ctx.audit.record(ctx.actor(), "remember", &mem.label, true, text.clone()); json!({ "remembered": text }) }
            Err(e) => { ctx.audit.record(ctx.actor(), "remember", &mem.label, false, e.to_string()); json!({ "error": e.to_string() }) }
        };
        out.into()
    })
}

fn recall(ctx: Arc<Ctx>) -> ToolFunc {
    tool_func!(async |args: Value| -> Value with [ctx = ctx.clone()] {
        let query = arg_str(&args, "query").to_string();
        let mem = ctx.mem.as_ref().expect("recall is registered only with mem");
        let store = mem.store.display().to_string();
        let out = match mem_run(mem, &["search", &store, &query]).await {
            Ok(text) => {
                let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                ctx.audit.record(ctx.actor(), "recall", &mem.label, true, format!("q={query:?} {} memories", lines.len()));
                json!({ "query": query, "memories": lines })
            }
            Err(e) => { ctx.audit.record(ctx.actor(), "recall", &mem.label, false, e.to_string()); json!({ "error": e.to_string() }) }
        };
        out.into()
    })
}
