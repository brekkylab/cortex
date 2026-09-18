//! `cortex-hyperclova` — HyperCLOVA X working over a Cortex tree, inside one actor's permissions.
//!
//! ```text
//! cortex-hyperclova --actor 구매팀 "이번 주 위험 거래처와 대체 후보를 정리해 주세요"
//! cortex-hyperclova --actor 인사팀 --tree-only
//! cortex-hyperclova --actor 재무팀 --s3 my-bucket/finance --s3-at 재무팀 "…"
//! ```
//!
//! The model is reached through CLOVA Studio's OpenAI-compatible endpoint, which ailoy already
//! speaks as its `ChatCompletion` schema; the tools it is given are the ones in [`tools`], each
//! answering through an [`acl::AclFs`] built for `--actor`. What the model reads, it read as
//! that actor; what it writes lands under `산출물/` with the readership its citations imply.
//!
//! Function Calling on `HCX-007` is accepted by CLOVA Studio only with reasoning turned off,
//! which the OpenAI-compatible request spells as `reasoning_effort: "none"`; that is sent by
//! default for that model and left out for every other, so `HCX-005` gets the plain request.

mod acl;
mod audit;
mod tools;
mod tree;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use ailoy::{
    agent::{Agent, AgentProvider, AgentSpec, get_agent_providers_mut},
    lang_model::{LangModelProvider, get_lm_providers_mut},
    message::{Message, Part, Role},
    tool::get_tool_providers_mut,
};
use anyhow::Context as _;
use chrono::Local;
use clap::Parser;
use cortex::fs::{DirentKind, FileSystem as _};
use futures::StreamExt as _;

use crate::{
    acl::{Acl, AclFs, readers_ko},
    audit::Audit,
    tools::{Ctx, Mem},
    tree::S3Source,
};

const DEFAULT_URL: &str = "https://clovastudio.stream.ntruss.com/v1/openai/chat/completions";
const PROVIDER: &str = "clovastudio";

#[derive(Parser, Debug)]
#[command(
    name = "cortex-hyperclova",
    about = "HyperCLOVA X over a Cortex tree, scoped to one actor's read permissions"
)]
struct Args {
    /// Who the agent works as; decides what the tree lets it read and cite.
    #[arg(long, default_value = "구매팀")]
    actor: String,

    /// CLOVA Studio model id: `HCX-005` or `HCX-007`.
    #[arg(long, default_value = "HCX-005")]
    model: String,

    /// The workspace root: one mount per top-level directory.
    #[arg(long, default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/procurement"))]
    workspace: PathBuf,

    /// Serve one top-level directory from an S3-compatible bucket instead: `bucket[/prefix]`.
    /// Credentials come from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`.
    #[arg(long)]
    s3: Option<String>,

    /// Which top-level directory `--s3` stands in for.
    #[arg(long, default_value = "재무팀")]
    s3_at: String,

    #[arg(long, default_value = "ap-northeast-2")]
    s3_region: String,

    /// A non-AWS endpoint, e.g. `https://kr.object.ncloudstorage.com` for Ncloud Object Storage.
    #[arg(long)]
    s3_endpoint: Option<String>,

    /// Path to cortex's `mem` executable; enables the `remember` / `recall` tools.
    /// Defaults to `target/debug/mem` next to this workspace's build output when present.
    #[arg(long)]
    mem_bin: Option<PathBuf>,

    /// `reasoning_effort` sent with every request. Defaults to `none` on HCX-007, which CLOVA
    /// Studio requires before it accepts `tools` on that model; unset elsewhere.
    #[arg(long)]
    reasoning_effort: Option<String>,

    /// Print the tree as the actor sees it and exit without calling the model.
    #[arg(long)]
    tree_only: bool,

    /// Show every tool result in full rather than its first lines.
    #[arg(long)]
    verbose: bool,

    /// What the agent is asked to do.
    #[arg(
        default_value = "협력사 신용등급 변동표·납기 이력·여신한도를 대조해 이번 주 위험 거래처와 대체 후보를 뽑고, 규정 근거와 인용 파일을 붙인 보고서로 저장해 주세요."
    )]
    question: String,
}

const INSTRUCTION: &str = "\
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let acl_text = std::fs::read_to_string(args.workspace.join("정책/acl.json"))
        .with_context(|| format!("reading {}/정책/acl.json", args.workspace.display()))?;
    let acl = Arc::new(Acl::from_json(&acl_text)?);

    let s3 = args
        .s3
        .as_deref()
        .map(|spec| S3Source::parse(spec, args.s3_region.clone(), args.s3_endpoint.clone()));
    let (workfs, mounted) = tree::build(
        &args.workspace,
        s3.as_ref().map(|s| (args.s3_at.as_str(), s)),
    )?;
    let fs = Arc::new(AclFs::new(workfs, acl.clone(), args.actor.clone()));

    banner(&args, &mounted);

    if args.tree_only {
        print_tree(fs.as_ref()).await?;
        return Ok(());
    }

    let api_key = std::env::var("CLOVASTUDIO_API_KEY")
        .context("CLOVASTUDIO_API_KEY is not set (a CLOVA Studio test or service API key)")?;
    let url = std::env::var("CLOVASTUDIO_OPENAI_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());

    let mem = resolve_mem(&args).await?;
    let audit = Audit::default();
    let ctx = Arc::new(Ctx {
        fs: fs.clone(),
        audit: audit.clone(),
        model: args.model.clone(),
        mem,
        seen: Default::default(),
    });

    // Three registries, one name: the model endpoint, the tools, and the pairing of the two.
    {
        let mut lmp = LangModelProvider::new();
        lmp.insert(
            "hyperclova/*".into(),
            LangModelProvider::chat_completion(&url, Some(api_key))?,
        );
        get_lm_providers_mut().insert(PROVIDER.into(), lmp);
        get_tool_providers_mut().insert(PROVIDER.into(), tools::provider(ctx.clone()));
        get_agent_providers_mut().insert(PROVIDER.into(), AgentProvider::new(PROVIDER, PROVIDER));
    }

    let instruction = format!(
        "{INSTRUCTION}\n\n현재 트리 — {} 기준 열람 가능 여부:\n{}",
        args.actor,
        tree_snapshot(fs.as_ref()).await?
    );
    let mut spec = AgentSpec::new(format!("hyperclova/{}", args.model))
        .instruction(instruction)
        .tools(tools::descs(ctx.mem.is_some()));
    if let Some(effort) = args
        .reasoning_effort
        .clone()
        .or_else(|| default_reasoning_effort(&args.model))
    {
        spec = spec.reasoning_effort(effort);
    }
    let mut agent = Agent::try_with_provider(spec, PROVIDER)?;

    println!("{DIM}질문{RESET}  {}", args.question);
    println!();

    let started = std::time::Instant::now();
    run_turn(&mut agent, &args.question, args.verbose).await?;
    // A turn that ends without a saved report is not finished; the audit log, not the model's
    // account of itself, says which it was.
    if !audit
        .entries()
        .iter()
        .any(|e| e.tool == "write_report" && e.allowed)
    {
        println!();
        println!("{YELLOW}※ write_report 가 호출되지 않았다 — 저장을 요청한다{RESET}");
        run_turn(
            &mut agent,
            "보고서가 아직 저장되지 않았다. 지금 write_report 를 호출해 산출물/ 아래에 저장한다. \
             sources 에는 이 실행에서 read 로 읽은 파일만 넣는다. 읽은 파일이 없으면 본문을 '작성 불가' 로 하고 sources 에는 읽은 정책·규정 파일을 넣는다.",
            args.verbose,
        )
        .await?;
    }
    println!();
    println!(
        "{DIM}{}s · 모델 {} · 사용자 {}{RESET}",
        started.elapsed().as_secs(),
        args.model,
        args.actor
    );

    print_audit(&audit);
    check_report_against_audit(fs.as_ref(), &audit).await;
    let log_path = Path::new("산출물").join(&args.actor).join(format!(
        "감사로그-{}.jsonl",
        Local::now().format("%Y%m%d-%H%M%S")
    ));
    tools::write_all(fs.as_ref(), &log_path, audit.to_jsonl().as_bytes()).await?;
    // The log names files the actor was refused; that is the actor's business and its
    // compliance owner's, not every department's.
    let side = serde_json::json!({ "readers": [args.actor], "author": args.actor, "model": args.model,
        "created": Local::now().to_rfc3339() });
    tools::write_all(
        fs.as_ref(),
        &acl::sidecar_of(&log_path),
        serde_json::to_string_pretty(&side)?.as_bytes(),
    )
    .await?;
    println!("{DIM}감사 로그 → {}{RESET}", log_path.display());
    Ok(())
}

/// The model's closing summary is its own account; this is the tree's. Every source the tree
/// refused is looked for, by path, in the report that was written.
async fn check_report_against_audit(fs: &AclFs<cortex::fs::WorkFs>, audit: &Audit) {
    let entries = audit.entries();
    let Some(report) = entries
        .iter()
        .rev()
        .find(|e| e.tool == "write_report" && e.allowed)
        .map(|e| PathBuf::from(&e.path))
    else {
        println!("{RED}보고서 없음 — write_report 가 성공한 기록이 없다{RESET}");
        return;
    };
    let body = match tools::read_all(fs, &report).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            println!("{RED}보고서를 다시 읽지 못했다: {e}{RESET}");
            return;
        }
    };
    let mut denied: Vec<&str> = entries
        .iter()
        .filter(|e| e.tool == "read" && !e.allowed && e.detail.contains("닫혀 있음"))
        .map(|e| e.path.as_str())
        .collect();
    denied.sort_unstable();
    denied.dedup();
    println!();
    println!("{BOLD}대조{RESET}  트리가 거절한 자료가 보고서에 적혀 있는가");
    if denied.is_empty() {
        println!("  거절된 자료 없음");
    }
    for d in denied {
        let mark = if body.contains(d) {
            format!("{GREEN}명시{RESET}")
        } else {
            format!("{RED}누락{RESET}")
        };
        println!("  {mark}  {d}");
    }
}

/// CLOVA Studio accepts Function Calling on `HCX-007` only with reasoning switched off; on the
/// OpenAI-compatible endpoint that is the standard `reasoning_effort` field set to `"none"`.
fn default_reasoning_effort(model: &str) -> Option<String> {
    model
        .eq_ignore_ascii_case("HCX-007")
        .then(|| "none".to_string())
}

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

fn banner(args: &Args, mounted: &[tree::Mounted]) {
    println!("{BOLD}Cortex tree{RESET}  {}", args.workspace.display());
    for m in mounted {
        println!("  /{:<8} ← {}", m.name, m.source);
    }
    println!(
        "{BOLD}actor{RESET}       {}    {BOLD}model{RESET} {}",
        args.actor, args.model
    );
    println!();
}

async fn print_tree(fs: &AclFs<cortex::fs::WorkFs>) -> anyhow::Result<()> {
    print_dir(fs, PathBuf::new(), 0).await
}

fn print_dir<'a>(
    fs: &'a AclFs<cortex::fs::WorkFs>,
    dir: PathBuf,
    depth: usize,
) -> futures::future::BoxFuture<'a, anyhow::Result<()>> {
    Box::pin(async move {
        let mut entries = fs.list(&dir).await?;
        entries.sort_by(|a, b| {
            (a.kind == DirentKind::File, &a.name).cmp(&(b.kind == DirentKind::File, &b.name))
        });
        for e in entries {
            if e.name.starts_with('.') {
                continue;
            }
            let p = dir.join(&e.name);
            let v = fs.verdict(&p);
            let indent = "  ".repeat(depth);
            match e.kind {
                DirentKind::Dir => {
                    println!(
                        "{indent}{BOLD}{}/{RESET}  {DIM}{} · {}{RESET}",
                        e.name,
                        v.label,
                        readers_ko(&v.readers)
                    );
                    print_dir(fs, p, depth + 1).await?;
                }
                DirentKind::File => {
                    let mark = if !v.readable {
                        format!("{RED}🔒 {} 전용{RESET}", readers_ko(&v.readers))
                    } else if !fs.may_read(&p).await {
                        format!("{RED}🔒 인용 원본의 권한을 물려받음{RESET}")
                    } else {
                        format!("{GREEN}열람{RESET}")
                    };
                    println!("{indent}{}  {mark}", e.name);
                }
            }
        }
        Ok(())
    })
}

/// The tree as `--tree-only` prints it, without colour: one line per entry with the actor's
/// access, for the system prompt.
async fn tree_snapshot(fs: &AclFs<cortex::fs::WorkFs>) -> anyhow::Result<String> {
    let mut out = String::new();
    let files = tools::walk(fs, Path::new("")).await?;
    for f in files {
        let v = fs.verdict(&f);
        let access = if fs.may_read(&f).await {
            "열람 가능".to_string()
        } else {
            format!(
                "🔒 권한 밖 — {} 전용 · 질문과 관련되면 read 를 시도해 거절 사유를 받을 것",
                readers_ko(&v.readers)
            )
        };
        out.push_str(&format!("- {}  [{}]\n", f.display(), access));
    }
    Ok(out)
}

async fn resolve_mem(args: &Args) -> anyhow::Result<Option<Mem>> {
    let bin = match &args.mem_bin {
        Some(b) => b.clone(),
        None => {
            let guess = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mem");
            if guess.exists() {
                guess
            } else {
                return Ok(None);
            }
        }
    };
    // One store per actor: what a department's agent concluded from files only it may read is
    // itself something only that department may recall.
    let dir = tree::output_dir(&args.workspace)?.join(&args.actor);
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
    Ok(Some(Mem { bin, store }))
}

fn show(msg: &Message, verbose: bool) {
    if verbose {
        let calls = msg.tool_calls.as_ref().map(Vec::len).unwrap_or(0);
        println!(
            "{DIM}-- {:?} message · {} parts · {} tool calls{RESET}",
            msg.role,
            msg.contents.len(),
            calls
        );
    }
    match msg.role {
        Role::Assistant => {
            if let Some(calls) = &msg.tool_calls {
                for c in calls {
                    if let Part::Function { function, .. } = c {
                        let a = serde_json::to_string(&function.arguments).unwrap_or_default();
                        println!(
                            "{CYAN}▶ {}{RESET} {DIM}{}{RESET}",
                            function.name,
                            clip(&a, 160)
                        );
                    }
                }
            }
            for part in &msg.contents {
                if let Part::Text { text } = part
                    && !text.trim().is_empty()
                {
                    println!();
                    println!("{text}");
                }
            }
        }
        Role::Tool => {
            for part in &msg.contents {
                let text = match part {
                    Part::Value { value } => serde_json::to_string(value).unwrap_or_default(),
                    Part::Text { text } => text.clone(),
                    _ => String::new(),
                };
                let colour = if text.contains("permission_denied") {
                    RED
                } else {
                    YELLOW
                };
                let shown = if verbose { text } else { clip(&text, 240) };
                println!("  {colour}◀{RESET} {DIM}{shown}{RESET}");
            }
        }
        _ => {}
    }
}

async fn run_turn(agent: &mut Agent, text: &str, verbose: bool) -> anyhow::Result<()> {
    let mut stream =
        agent.run(Message::new(Role::User).with_contents([Part::text(text.to_string())]));
    while let Some(out) = stream.next().await {
        let out = out?;
        show(&out.message, verbose);
    }
    Ok(())
}

fn clip(s: &str, n: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= n {
        one_line
    } else {
        format!("{}…", one_line.chars().take(n).collect::<String>())
    }
}

fn print_audit(audit: &Audit) {
    println!();
    println!("{BOLD}감사 로그{RESET}  (사용자 · 도구 · 경로 · 판정)");
    for e in audit.entries() {
        let verdict = if e.allowed {
            format!("{GREEN}허용{RESET}")
        } else {
            format!("{RED}거절{RESET}")
        };
        println!(
            "  {}  {:<6} {:<12} {:<44} {}  {DIM}{}{RESET}",
            e.at.format("%H:%M:%S"),
            e.actor,
            e.tool,
            clip(&e.path, 44),
            verdict,
            clip(&e.detail, 60)
        );
    }
}
