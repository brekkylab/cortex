//! `cortex-hyperclova` — HyperCLOVA X working over a Cortex tree, inside one actor's permissions.
//!
//! ```text
//! cortex-hyperclova --actor 구매팀 "이번 주 위험 거래처와 대체 후보를 정리해 주세요"
//! cortex-hyperclova --actor 인사팀 --tree-only
//! cortex-hyperclova --actor 재무팀 --model HCX-007 --s3 my-bucket/finance --s3-at 재무팀 "…"
//! ```
//!
//! A terminal in front of [`cortex_agent_hyperclova::session::run`]: the same events a window
//! renders, printed one per line.

use std::{path::PathBuf, sync::Arc};

use anyhow::Context as _;
use clap::Parser;
use cortex_agent_hyperclova::{
    session::{self, Config, Event},
    tree::S3Source,
};

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
    #[arg(default_value = session::DEFAULT_QUESTION)]
    question: String,
}

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let s3 = args.s3.as_deref().map(|spec| {
        (
            args.s3_at.clone(),
            S3Source::parse(spec, args.s3_region.clone(), args.s3_endpoint.clone()),
        )
    });

    if args.tree_only {
        let (fs, mounts) = session::open_tree(&args.workspace, &args.actor, s3.as_ref())?;
        banner(&args, &mounts);
        for n in session::tree_nodes(fs.as_ref()).await? {
            let indent = "  ".repeat(n.depth);
            match n.kind {
                "dir" => println!(
                    "{indent}{BOLD}{}/{RESET}  {DIM}{} · {}{RESET}",
                    n.name, n.label, n.readers
                ),
                _ => {
                    let mark = match n.access {
                        "open" => format!("{GREEN}열람{RESET}"),
                        "inherited" => format!("{RED}🔒 인용 자료의 권한을 따름{RESET}"),
                        _ => format!("{RED}🔒 {} 전용{RESET}", n.readers),
                    };
                    println!("{indent}{}  {mark}", n.name);
                }
            }
        }
        return Ok(());
    }

    let cfg = Config {
        workspace: args.workspace.clone(),
        actor: args.actor.clone(),
        model: args.model.clone(),
        question: args.question.clone(),
        s3,
        api_key: std::env::var("CLOVASTUDIO_API_KEY")
            .context("CLOVASTUDIO_API_KEY is not set (a CLOVA Studio test or service API key)")?,
        url: std::env::var("CLOVASTUDIO_OPENAI_URL")
            .unwrap_or_else(|_| session::DEFAULT_URL.to_string()),
        mem_bin: args.mem_bin.clone(),
        reasoning_effort: args.reasoning_effort.clone(),
    };
    let verbose = args.verbose;
    let workspace = args.workspace.display().to_string();
    let sink: session::Sink = Arc::new(move |ev| print_event(ev, verbose, &workspace));
    session::run(cfg, sink).await
}

fn banner(args: &Args, mounts: &[session::Mount]) {
    println!("{BOLD}Cortex tree{RESET}  {}", args.workspace.display());
    for m in mounts {
        println!("  /{:<8} ← {}", m.name, m.source);
    }
    println!(
        "{BOLD}actor{RESET}       {}    {BOLD}model{RESET} {}",
        args.actor, args.model
    );
    println!();
}

fn print_event(ev: Event, verbose: bool, workspace: &str) {
    match ev {
        Event::Started {
            actor,
            model,
            question,
            mounts,
        } => {
            println!("{BOLD}Cortex tree{RESET}  {workspace}");
            for m in &mounts {
                println!("  /{:<8} ← {}", m.name, m.source);
            }
            println!("{BOLD}actor{RESET}       {actor}    {BOLD}model{RESET} {model}");
            println!();
            println!("{DIM}질문{RESET}  {question}");
            println!();
        }
        Event::Assistant { text, calls } => {
            for c in calls {
                let a = serde_json::to_string(&c.arguments).unwrap_or_default();
                println!("{CYAN}▶ {}{RESET} {DIM}{}{RESET}", c.name, clip(&a, 160));
            }
            if let Some(t) = text {
                println!();
                println!("{t}");
            }
        }
        Event::ToolResult { value, denied } => {
            let text = serde_json::to_string(&value).unwrap_or_default();
            let colour = if denied { RED } else { YELLOW };
            let shown = if verbose { text } else { clip(&text, 240) };
            println!("  {colour}◀{RESET} {DIM}{shown}{RESET}");
        }
        Event::Notice { text } => {
            println!();
            println!("{YELLOW}※ {text}{RESET}");
        }
        Event::Audit { .. } => {}
        Event::Check { report, denied } => {
            println!();
            println!("{BOLD}대조{RESET}  트리가 거절한 자료가 보고서에 적혀 있는가");
            if report.is_none() {
                println!("  {RED}보고서 없음 — write_report 가 성공한 기록이 없다{RESET}");
            }
            if denied.is_empty() {
                println!("  거절된 자료 없음");
            }
            for d in denied {
                let mark = if d.mentioned {
                    format!("{GREEN}명시{RESET}")
                } else {
                    format!("{RED}누락{RESET}")
                };
                println!("  {mark}  {}", d.path);
            }
        }
        Event::Finished { seconds, log } => {
            println!("{DIM}{seconds:.0}s · 감사 로그 → {log}{RESET}");
        }
    }
}

fn clip(s: &str, n: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= n {
        one_line
    } else {
        format!("{}…", one_line.chars().take(n).collect::<String>())
    }
}
