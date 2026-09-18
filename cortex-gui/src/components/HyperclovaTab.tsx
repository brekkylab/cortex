// The run screen: one actor, one model, one request — and the run itself as it happens.
//
// Left: who is asking, which model answers, and the tree as that actor (or the administrator)
// sees it. Middle: the request and the run log — each call the model made, what the tree
// answered, what the model said. Right: the record — the report and who may read it, every
// tool call with its verdict, and the closing check of refused sources against the report.
// All of it arrives as `hcx` events from the Rust side.

import { type ReactElement, useCallback, useEffect, useRef, useState } from "react";

import { hcxConfig, hcxRead, hcxRun, hcxTree, messageOf, onHcxEvent } from "../api";
import type { HcxAuditEntry, HcxConfig, HcxEvent, HcxNode } from "../types";
import { CheckIcon, ChevronIcon, CrossIcon, FileIcon, FolderIcon, LockIcon, ToolIcon } from "./icons";

interface Props {
  active: boolean;
  notify: (text: string, tone?: "ok" | "error") => void;
  /** Called when a run finishes, so the workspace tab can re-list what the agent wrote. */
  onFinished: () => void;
  /** A model chosen on the agent tab, applied when it changes. */
  presetModel: string | null;
}

/** The actor every rule opens to. Views the tree; never runs the agent. */
const ADMIN = "관리자";

const SCOPE: Record<string, string> = {
  구매팀: "협력사 평가 · 발주 · 규정",
  재무팀: "여신 한도 · 실적 · 협력사 평가",
  인사팀: "채용 · 회의록 · 정책",
};

type Entry =
  | { kind: "request"; text: string }
  | { kind: "call"; name: string; args: string; result?: { summary: string; text: string; denied: boolean } }
  | { kind: "message"; text: string }
  | { kind: "system"; text: string }
  | { kind: "error"; text: string };

interface Check {
  report: string | null;
  denied: { path: string; mentioned: boolean }[];
}

export default function HyperclovaTab(props: Props) {
  const { notify, onFinished } = props;

  const [config, setConfig] = useState<HcxConfig | null>(null);
  const [actor, setActor] = useState("구매팀");
  const [model, setModel] = useState("HCX-007");
  const [request, setRequest] = useState("");
  const [view, setView] = useState<"actor" | "admin">("actor");
  const [tree, setTree] = useState<HcxNode[]>([]);
  const [log, setLog] = useState<Entry[]>([]);
  const [audit, setAudit] = useState<HcxAuditEntry[]>([]);
  const [check, setCheck] = useState<Check | null>(null);
  const [report, setReport] = useState<{ path: string; text: string } | null>(null);
  const [running, setRunning] = useState(false);
  const [elapsed, setElapsed] = useState<number | null>(null);
  const [startedAt, setStartedAt] = useState<number | null>(null);
  const [now, setNow] = useState(Date.now());
  const [pane, setPane] = useState<"report" | "audit" | "check" | "file">("report");
  const [file, setFile] = useState<{ path: string; text: string | null; error: string | null } | null>(null);
  const [mounts, setMounts] = useState<{ name: string; source: string }[]>([]);
  const logRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    hcxConfig().then(
      (c) => {
        setConfig(c);
        setRequest(c.default_question);
        if (c.models.length) setModel(c.models[0]);
      },
      (err) => notify(messageOf(err), "error"),
    );
  }, [notify]);

  useEffect(() => {
    if (props.presetModel) setModel(props.presetModel);
  }, [props.presetModel]);

  const loadTree = useCallback(
    (who: string) => hcxTree(who).then(setTree, (err) => notify(messageOf(err), "error")),
    [notify],
  );
  const viewer = view === "admin" ? ADMIN : actor;
  useEffect(() => {
    void loadTree(viewer);
  }, [viewer, loadTree]);

  const viewerRef = useRef(viewer);
  viewerRef.current = viewer;
  const actorRef = useRef(actor);
  actorRef.current = actor;

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    onHcxEvent((ev: HcxEvent) => {
      if (cancelled) return;
      switch (ev.kind) {
        case "started":
          setMounts(ev.mounts);
          break;
        case "assistant":
          setLog((prev) => [
            ...prev,
            ...ev.calls.map<Entry>((c) => ({ kind: "call", name: c.name, args: argsOf(c.arguments) })),
            ...(ev.text ? [{ kind: "message", text: ev.text } as Entry] : []),
          ]);
          break;
        case "tool_result":
          // A result belongs to the earliest call still waiting for one.
          setLog((prev) => {
            const next = [...prev];
            const i = next.findIndex((e) => e.kind === "call" && !e.result);
            if (i >= 0) {
              const call = next[i] as Extract<Entry, { kind: "call" }>;
              next[i] = { ...call, result: { summary: summarize(ev.value), text: JSON.stringify(ev.value, null, 2), denied: ev.denied } };
            }
            return next;
          });
          break;
        case "notice":
          setLog((prev) => [...prev, { kind: "system", text: ev.text }]);
          break;
        case "audit":
          setAudit((prev) => [...prev, ev.entry]);
          break;
        case "check":
          setCheck({ report: ev.report, denied: ev.denied });
          if (ev.report) {
            const path = ev.report;
            hcxRead(actorRef.current, path).then(
              (text) => setReport({ path, text }),
              (err) => setReport({ path, text: messageOf(err) }),
            );
          }
          break;
        case "finished":
          setElapsed(ev.seconds);
          setRunning(false);
          void loadTree(viewerRef.current);
          onFinished();
          break;
        case "failed":
          setLog((prev) => [...prev, { kind: "error", text: ev.message }]);
          setRunning(false);
          break;
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [loadTree, onFinished]);

  useEffect(() => {
    logRef.current?.scrollTo({ top: logRef.current.scrollHeight, behavior: "smooth" });
  }, [log]);

  useEffect(() => {
    if (!running) return;
    const t = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(t);
  }, [running]);

  const start = useCallback(async () => {
    if (running || !request.trim()) return;
    setLog([{ kind: "request", text: request }]);
    setAudit([]);
    setCheck(null);
    setReport(null);
    setElapsed(null);
    setPane("report");
    setStartedAt(Date.now());
    setRunning(true);
    try {
      await hcxRun(actor, model, request);
    } catch (err) {
      setRunning(false);
      notify(messageOf(err), "error");
    }
  }, [running, request, actor, model, notify]);

  // ⌘↩ anywhere on this screen starts the run.
  useEffect(() => {
    if (!props.active) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        e.stopPropagation();
        if (document.activeElement instanceof HTMLButtonElement) document.activeElement.blur();
        void start();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [props.active, start]);

  const openFile = useCallback(
    (path: string) => {
      setPane("file");
      setFile({ path, text: null, error: null });
      hcxRead(actor, path).then(
        (text) => setFile({ path, text, error: null }),
        (err) => setFile({ path, text: null, error: messageOf(err) }),
      );
    },
    [actor],
  );

  const files = tree.filter((n) => n.kind === "file");
  const readable = files.filter((n) => n.access === "open").length;
  const canRun = !running && request.trim().length > 0 && (config?.api_key_present ?? true);
  const live = startedAt ? Math.max(0, Math.round((now - startedAt) / 1000)) : 0;
  const status = running
    ? `실행 중 · ${live}초`
    : elapsed !== null
      ? `완료 · ${elapsed.toFixed(0)}초`
      : config && !config.api_key_present
        ? "CLOVASTUDIO_API_KEY 없음"
        : "대기";

  return (
    <>
      <aside className="run-side">
        <section>
          <h4>사용자</h4>
          <div className="actor-list" role="radiogroup">
            {(config?.actors ?? Object.keys(SCOPE)).map((a) => (
              <button key={a} role="radio" aria-checked={a === actor} className="actor" disabled={running} onClick={() => setActor(a)}>
                <span className="avatar">{a[0]}</span>
                <span className="text">
                  <span className="name">{a}</span>
                  <span className="scope">{SCOPE[a] ?? ""}</span>
                </span>
              </button>
            ))}
          </div>
        </section>

        <section>
          <h4>모델</h4>
          <div className="segmented">
            {(config?.models ?? ["HCX-007", "HCX-005"]).map((m) => (
              <button key={m} aria-pressed={m === model} disabled={running} onClick={() => setModel(m)}>
                {m}
              </button>
            ))}
          </div>
        </section>

        <section className="grow">
          <h4>
            트리
            <span className="spacer" />
            <div className="segmented mini">
              <button aria-pressed={view === "actor"} onClick={() => setView("actor")}>
                {actor}
              </button>
              <button aria-pressed={view === "admin"} onClick={() => setView("admin")}>
                관리자
              </button>
            </div>
          </h4>
          <div className="meta">{view === "actor" ? `열람 가능 ${readable} / ${files.length}` : "모든 파일과 열람 부서"}</div>
          <div className="tree-list">
            {tree.map((n) => (
              <div
                key={n.path}
                className={`node ${n.kind} ${n.access} ${file?.path === n.path ? "selected" : ""}`}
                style={{ paddingLeft: 10 + n.depth * 14 }}
                title={n.path}
                onClick={() => n.kind === "file" && openFile(n.path)}
              >
                <span className="glyph">
                  {n.access === "open" ? n.kind === "dir" ? <FolderIcon open /> : <FileIcon /> : <LockIcon />}
                </span>
                <span className="name">{n.name}</span>
                <span className="who">{view === "admin" ? n.readers : n.access === "inherited" ? "권한 승계" : n.access === "locked" ? `${n.readers} 전용` : ""}</span>
              </div>
            ))}
          </div>
        </section>

        {mounts.length > 0 && (
          <section>
            <h4>연결</h4>
            <div className="mount-list">
              {mounts.map((m) => (
                <div key={m.name} className="mnt" title={m.source}>
                  <span className="name">/{m.name}</span>
                  <span className="detail">{sourceLabel(m.source)}</span>
                </div>
              ))}
            </div>
          </section>
        )}
      </aside>

      <section className="run-main">
        <div className="composer">
          <textarea rows={3} value={request} disabled={running} placeholder="요청" onChange={(e) => setRequest(e.target.value)} />
          <div className="bar">
            <span className={`status ${running ? "live" : ""}`}>
              {running && <span className="dot" />}
              {status}
            </span>
            <span className="spacer" />
            <button className="primary" disabled={!canRun} onClick={() => void start()}>
              실행
              <kbd>⌘↩</kbd>
            </button>
          </div>
        </div>

        <div className="run-log" ref={logRef}>
          {log.length === 0 && <div className="empty">실행 결과가 여기에 표시됩니다.</div>}
          {log.length > 0 && (
            <div className="task-head">
              <span className="title">
                {actor} · {model}
              </span>
              <span className="spacer" />
              <span className="time">{running ? `${live}초 경과` : elapsed !== null ? `${elapsed.toFixed(0)}초` : ""}</span>
            </div>
          )}
          {log.map((e, i) => {
            switch (e.kind) {
              case "request":
                return (
                  <div className="entry request" key={i}>
                    <span className="who">{actor}</span>
                    <p>{e.text}</p>
                  </div>
                );
              case "call":
                return (
                  <details className={`entry call ${e.result ? (e.result.denied ? "denied" : "done") : "pending"}`} key={i}>
                    <summary>
                      <ChevronIcon open={false} />
                      <ToolIcon />
                      <code className="fn">{e.name}</code>
                      <code className="args">{e.args}</code>
                      <span className="spacer" />
                      {e.result ? (
                        <span className={`verdict ${e.result.denied ? "no" : "ok"}`}>
                          {e.result.denied ? <LockIcon /> : <CheckIcon />}
                          {e.result.denied ? "거절" : e.result.summary}
                        </span>
                      ) : (
                        <span className="verdict wait">…</span>
                      )}
                    </summary>
                    {e.result && <pre>{e.result.text}</pre>}
                  </details>
                );
              case "message":
                return (
                  <div className="entry message" key={i}>
                    <span className="who">{model}</span>
                    <Markdown text={e.text} />
                  </div>
                );
              case "system":
                return (
                  <div className="entry system" key={i}>
                    {e.text}
                  </div>
                );
              case "error":
                return (
                  <div className="entry error" key={i}>
                    <CrossIcon /> {e.text}
                  </div>
                );
            }
          })}
        </div>
      </section>

      <aside className="run-record">
        <div className="segmented tabs-row" role="tablist">
          <button role="tab" aria-pressed={pane === "report"} onClick={() => setPane("report")}>
            보고서
          </button>
          <button role="tab" aria-pressed={pane === "audit"} onClick={() => setPane("audit")}>
            감사 로그{audit.length > 0 && <span className="count">{audit.length}</span>}
          </button>
          <button role="tab" aria-pressed={pane === "check"} onClick={() => setPane("check")}>
            권한 대조{check && check.denied.length > 0 && <span className="count">{check.denied.length}</span>}
          </button>
          {file && (
            <button role="tab" aria-pressed={pane === "file"} onClick={() => setPane("file")}>
              파일
            </button>
          )}
        </div>

        {pane === "file" && file && (
          <div className="record-body">
            <div className="path">{file.path}</div>
            {file.text === null && file.error === null && <div className="empty">여는 중</div>}
            {file.error && (
              <div className="check-row no">
                <LockIcon />
                <span>{file.error}</span>
              </div>
            )}
            {file.text !== null && (file.path.endsWith(".md") ? <Markdown text={file.text} /> : <pre className="file-text">{file.text}</pre>)}
          </div>
        )}

        {pane === "report" && (
          <div className="record-body">
            {!report && <div className="empty">{running ? "보고서 작성 중" : "저장된 보고서가 없습니다."}</div>}
            {report && (
              <>
                <div className="path">{report.path}</div>
                <AclLine text={report.text} />
                <Markdown text={stripFooter(report.text)} />
              </>
            )}
          </div>
        )}

        {pane === "audit" && (
          <div className="record-body audit">
            {audit.length === 0 && <div className="empty">기록이 없습니다.</div>}
            {audit.length > 0 && (
              <table>
                <thead>
                  <tr>
                    <th>시각</th>
                    <th>도구</th>
                    <th>경로</th>
                    <th>판정</th>
                  </tr>
                </thead>
                <tbody>
                  {audit.map((e, i) => (
                    <tr key={i} className={e.allowed ? "ok" : "no"} title={e.detail}>
                      <td className="mono">{e.at.slice(11, 19)}</td>
                      <td className="mono">{e.tool}</td>
                      <td className="mono path">{e.path}</td>
                      <td className="verdict">{e.allowed ? "허용" : "거절"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        )}

        {pane === "check" && (
          <div className="record-body">
            {!check && <div className="empty">실행이 끝나면 거절된 자료가 보고서에 명시되었는지 대조합니다.</div>}
            {check && (
              <>
                <p className="lead">트리가 거절한 자료가 보고서에 적혀 있는지 경로로 확인한 결과입니다.</p>
                {!check.report && (
                  <div className="check-row no">
                    <CrossIcon /> 저장된 보고서가 없습니다.
                  </div>
                )}
                {check.denied.length === 0 && <div className="check-row">거절된 자료가 없습니다.</div>}
                {check.denied.map((d) => (
                  <div className={`check-row ${d.mentioned ? "ok" : "no"}`} key={d.path}>
                    {d.mentioned ? <CheckIcon /> : <CrossIcon />}
                    <span className="mono">{d.path}</span>
                    <span className="spacer" />
                    <span>{d.mentioned ? "명시" : "누락"}</span>
                  </div>
                ))}
              </>
            )}
          </div>
        )}
      </aside>
    </>
  );
}

/** Where a mount comes from, said briefly: the store's name, or the folder's own name. */
function sourceLabel(source: string): string {
  if (source.startsWith("Naver Cloud Storage")) return source;
  const path = source.replace(/\s+\(로컬 폴더\)$/, "");
  return `로컬 폴더 · ${path.split("/").filter(Boolean).pop() ?? path}`;
}

function argsOf(args: unknown): string {
  if (args && typeof args === "object") {
    const v = args as Record<string, unknown>;
    if (typeof v.path === "string" && Object.keys(v).length === 1) return v.path;
    if (typeof v.query === "string" && Object.keys(v).length === 1) return `"${v.query}"`;
    if (typeof v.path === "string" && typeof v.content === "string") return `${v.path} · ${v.content.length}자`;
  }
  return JSON.stringify(args);
}

function summarize(value: unknown): string {
  if (!value || typeof value !== "object") return "";
  const v = value as Record<string, unknown>;
  if (typeof v.written === "string") return `저장 · 열람 ${v.readers}`;
  if (Array.isArray(v.entries)) return `${v.entries.length}개`;
  if (typeof v.content === "string") return `${v.content.length}자`;
  if (Array.isArray(v.hits)) return `${v.hits.length}건`;
  if (Array.isArray(v.memories)) return `${v.memories.length}건`;
  if (typeof v.remembered === "string") return "기록";
  if (typeof v.error === "string") return String(v.error);
  return "";
}

function AclLine({ text }: { text: string }) {
  const readers = /열람 권한: ([^—\n]+)/.exec(text)?.[1]?.trim();
  const cited = /인용: (.+)/.exec(text)?.[1]?.split(",").map((s) => s.trim()).filter(Boolean) ?? [];
  if (!readers) return null;
  return (
    <div className="acl-line">
      <span className={`chip ${readers === "전 부서" ? "" : "locked"}`}>
        {readers !== "전 부서" && <LockIcon />}
        열람 {readers}
      </span>
      <span className="chip">인용 {cited.length}</span>
      <span className="note">인용 자료 기준 최소 권한</span>
    </div>
  );
}

function stripFooter(text: string): string {
  const i = text.lastIndexOf("\n---\n");
  return i > 0 ? text.slice(0, i) : text;
}

/** Enough Markdown for a report: headings, lists, tables, bold, inline code. */
function Markdown({ text }: { text: string }) {
  const lines = text.split("\n");
  const out: ReactElement[] = [];
  const PATH = /((?:[가-힣A-Za-z0-9_.-]+\/)+[가-힣A-Za-z0-9_.-]+\.(?:md|csv|json|txt|jsonl|sqlite))/;
  const inline = (s: string) =>
    s.split(/(\*\*[^*]+\*\*|`[^`]+`)/).flatMap((part, k) => {
      if (part.startsWith("**")) return [<strong key={k}>{part.slice(2, -2)}</strong>];
      if (part.startsWith("`")) return [<code key={k}>{part.slice(1, -1)}</code>];
      return part.split(PATH).map((piece, j) => (PATH.test(piece) ? <code className="path" key={`${k}-${j}`}>{piece}</code> : piece));
    });
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (line.startsWith("|")) {
      const rows: string[][] = [];
      while (i < lines.length && lines[i].startsWith("|")) {
        const cells = lines[i].slice(1).replace(/\|\s*$/, "").split("|").map((c) => c.trim());
        if (!cells.every((c) => /^:?-{2,}:?$/.test(c))) rows.push(cells);
        i++;
      }
      out.push(
        <table key={out.length}>
          <tbody>
            {rows.map((r, ri) => (
              <tr key={ri}>{r.map((c, ci) => (ri === 0 ? <th key={ci}>{inline(c)}</th> : <td key={ci}>{inline(c)}</td>))}</tr>
            ))}
          </tbody>
        </table>,
      );
      continue;
    }
    const h = /^(#{1,4})\s+(.*)$/.exec(line);
    if (h) {
      out.push(h[1].length <= 2 ? <h3 key={out.length}>{inline(h[2])}</h3> : <h4 key={out.length}>{inline(h[2])}</h4>);
      i++;
      continue;
    }
    const bullet = /^(\s*)[-*•]\s+(.*)$/;
    if (bullet.test(line)) {
      const items: { depth: number; text: string }[] = [];
      while (i < lines.length && bullet.test(lines[i])) {
        const m = bullet.exec(lines[i])!;
        items.push({ depth: Math.floor(m[1].length / 2), text: m[2] });
        i++;
      }
      out.push(
        <ul key={out.length}>
          {items.map((it, k) => (
            <li key={k} style={{ marginLeft: it.depth * 14 }}>
              {inline(it.text)}
            </li>
          ))}
        </ul>,
      );
      continue;
    }
    if (/^\s*\d+\.\s+/.test(line)) {
      const items: string[] = [];
      while (i < lines.length && /^\s*\d+\.\s+/.test(lines[i])) {
        items.push(lines[i].replace(/^\s*\d+\.\s+/, ""));
        i++;
      }
      out.push(
        <ol key={out.length}>
          {items.map((it, k) => (
            <li key={k}>{inline(it)}</li>
          ))}
        </ol>,
      );
      continue;
    }
    if (line.trim() !== "") out.push(<p key={out.length}>{inline(line)}</p>);
    i++;
  }
  return <div className="md">{out}</div>;
}
