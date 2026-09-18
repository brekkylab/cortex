// The run screen: one actor, one model, one request — and every run, past and present.
//
// Left: who is asking, which model answers, the runs so far, and the tree as that actor (or the
// administrator) sees it. Middle: the request and the run itself, step by step. Right: the
// record — the report and who may read it, every tool call with its verdict, the check of
// refused sources against the report, and a file opened from the tree. A live run arrives as
// `hcx` events; a past run is the same events read back from its record, so both go through
// one reducer.

import { type ReactElement, useCallback, useEffect, useMemo, useRef, useState } from "react";

import { hcxConfig, hcxRead, hcxRun, hcxRunDetail, hcxRuns, hcxTree, messageOf, onHcxEvent } from "../api";
import type { HcxAuditEntry, HcxConfig, HcxEvent, HcxNode, HcxRunSummary } from "../types";
import { CheckIcon, ChevronIcon, CrossIcon, DocIcon, FileIcon, FolderIcon, LockIcon, PenIcon, SearchIcon, SparkIcon } from "./icons";

interface Props {
  active: boolean;
  notify: (text: string, tone?: "ok" | "error") => void;
  onFinished: () => void;
  presetModel: string | null;
}

/** The actor every rule opens to. Views the tree; never runs the agent. */
const ADMIN = "관리자";

const SCOPE: Record<string, string> = {
  구매팀: "협력사 평가 · 발주 · 규정",
  재무팀: "여신 한도 · 실적 · 협력사 평가",
  인사팀: "채용 · 회의록 · 정책",
};

// ── the run as the screen keeps it ─────────────────────────────────────────

interface Step {
  name: string;
  args: Record<string, unknown>;
  result?: { value: unknown; denied: boolean };
}
type Entry =
  | { kind: "request"; text: string }
  | { kind: "step"; step: Step }
  | { kind: "message"; text: string }
  | { kind: "system"; text: string }
  | { kind: "error"; text: string };

interface RunState {
  actor: string;
  model: string;
  log: Entry[];
  audit: HcxAuditEntry[];
  check: { report: string | null; denied: { path: string; mentioned: boolean }[] } | null;
  mounts: { name: string; source: string }[];
  seconds: number | null;
  failed: string | null;
}

const empty = (actor: string, model: string, question: string): RunState => ({
  actor,
  model,
  log: [{ kind: "request", text: question }],
  audit: [],
  check: null,
  mounts: [],
  seconds: null,
  failed: null,
});

function reduce(state: RunState, ev: HcxEvent): RunState {
  switch (ev.kind) {
    case "started":
      return { ...state, actor: ev.actor, model: ev.model, mounts: ev.mounts };
    case "assistant": {
      const log = [...state.log];
      for (const c of ev.calls) log.push({ kind: "step", step: { name: c.name, args: (c.arguments ?? {}) as Record<string, unknown> } });
      if (ev.text) log.push({ kind: "message", text: ev.text });
      return { ...state, log };
    }
    case "tool_result": {
      // A result belongs to the earliest step still waiting for one.
      const log = [...state.log];
      const i = log.findIndex((e) => e.kind === "step" && !e.step.result);
      if (i >= 0) {
        const e = log[i] as Extract<Entry, { kind: "step" }>;
        log[i] = { kind: "step", step: { ...e.step, result: { value: ev.value, denied: ev.denied } } };
      }
      return { ...state, log };
    }
    case "notice":
      return { ...state, log: [...state.log, { kind: "system", text: ev.text }] };
    case "audit":
      return { ...state, audit: [...state.audit, ev.entry] };
    case "check":
      return { ...state, check: { report: ev.report, denied: ev.denied } };
    case "finished":
      return { ...state, seconds: ev.seconds };
    case "failed":
      return { ...state, failed: ev.message, log: [...state.log, { kind: "error", text: ev.message }] };
  }
}

// ── steps, said the way a person would ────────────────────────────────────

const str = (v: unknown) => (typeof v === "string" ? v : "");
const leaf = (p: string) => p.split("/").filter(Boolean).pop() ?? p;

/** What a step is doing, and what it did. */
function describe(step: Step): { icon: ReactElement; doing: string; done: string } {
  const { name, args, result } = step;
  const v = (result?.value ?? {}) as Record<string, unknown>;
  const path = str(args.path);
  switch (name) {
    case "read": {
      const n = typeof v.content === "string" ? `${v.content.length}자` : "";
      return { icon: <FileIcon />, doing: `${leaf(path)} 읽는 중`, done: result?.denied ? `${leaf(path)} 열 수 없음` : `${leaf(path)} 읽음${n ? ` · ${n}` : ""}` };
    }
    case "ls": {
      const n = Array.isArray(v.entries) ? `${v.entries.length}개` : "";
      const where = path ? leaf(path) : "루트";
      return { icon: <FolderIcon open />, doing: `${where} 폴더 여는 중`, done: result?.denied ? `${where} 폴더 열 수 없음` : `${where} 폴더 확인${n ? ` · ${n}` : ""}` };
    }
    case "search": {
      const q = str(args.query);
      const n = Array.isArray(v.hits) ? `${v.hits.length}건` : "";
      return { icon: <SearchIcon />, doing: `"${q}" 찾는 중`, done: `"${q}" 검색${n ? ` · ${n}` : ""}` };
    }
    case "write_report": {
      const readers = str(v.readers);
      return { icon: <DocIcon />, doing: "보고서 쓰는 중", done: v.error ? "보고서 저장 실패" : `보고서 저장${readers ? ` · 열람 ${readers}` : ""}` };
    }
    case "recall": {
      const n = Array.isArray(v.memories) ? v.memories.length : 0;
      return { icon: <SparkIcon />, doing: "이전 결론 확인 중", done: n ? `이전 결론 ${n}건 확인` : "이전 결론 없음" };
    }
    case "remember":
      return { icon: <PenIcon />, doing: "결론 기록 중", done: "결론 기록" };
    default:
      return { icon: <SparkIcon />, doing: `${name} 실행 중`, done: name };
  }
}

function deniedReason(step: Step): string {
  const v = (step.result?.value ?? {}) as Record<string, unknown>;
  const m = str(v.message);
  const who = /은\(는\) (.+?) 열람/.exec(m)?.[1];
  return who ? `${who} 전용` : m;
}

// ── the screen ────────────────────────────────────────────────────────────

export default function HyperclovaTab(props: Props) {
  const { notify, onFinished } = props;

  const [config, setConfig] = useState<HcxConfig | null>(null);
  const [actor, setActor] = useState("구매팀");
  const [model, setModel] = useState("HCX-007");
  const [request, setRequest] = useState("");
  const [view, setView] = useState<"actor" | "admin">("actor");
  const [tree, setTree] = useState<HcxNode[]>([]);
  const [runs, setRuns] = useState<HcxRunSummary[]>([]);
  const [current, setCurrent] = useState<RunState | null>(null);
  const [selectedRun, setSelectedRun] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const [startedAt, setStartedAt] = useState<number | null>(null);
  const [now, setNow] = useState(Date.now());
  const [report, setReport] = useState<{ path: string; text: string } | null>(null);
  const [file, setFile] = useState<{ path: string; text: string | null; error: string | null } | null>(null);
  const [pane, setPane] = useState<"report" | "audit" | "check" | "file">("report");
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

  const loadRuns = useCallback(() => hcxRuns().then(setRuns, (err) => notify(messageOf(err), "error")), [notify]);
  useEffect(() => {
    void loadRuns();
  }, [loadRuns]);
  const openedLatest = useRef(false);
  useEffect(() => {
    if (!config?.open_latest || openedLatest.current || runs.length === 0) return;
    openedLatest.current = true;
    openRun(runs[0].id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [config, runs]);

  const loadTree = useCallback((who: string) => hcxTree(who).then(setTree, (err) => notify(messageOf(err), "error")), [notify]);
  const viewer = view === "admin" ? ADMIN : actor;
  useEffect(() => {
    void loadTree(viewer);
  }, [viewer, loadTree]);
  const viewerRef = useRef(viewer);
  viewerRef.current = viewer;

  // A written report is read back as the actor who wrote it.
  const showReport = useCallback((who: string, path: string | null) => {
    if (!path) {
      setReport(null);
      return;
    }
    hcxRead(who, path).then(
      (text) => setReport({ path, text }),
      (err) => setReport({ path, text: messageOf(err) }),
    );
  }, []);

  // Live events, one subscription for the life of the window.
  const currentRef = useRef(current);
  currentRef.current = current;
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    onHcxEvent((ev: HcxEvent) => {
      if (cancelled) return;
      setCurrent((prev) => (prev ? reduce(prev, ev) : prev));
      if (ev.kind === "check") showReport(currentRef.current?.actor ?? actor, ev.report);
      if (ev.kind === "finished" || ev.kind === "failed") {
        setRunning(false);
        void loadTree(viewerRef.current);
        void loadRuns();
        if (ev.kind === "finished") onFinished();
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [loadTree, loadRuns, onFinished, showReport, actor]);

  useEffect(() => {
    logRef.current?.scrollTo({ top: logRef.current.scrollHeight, behavior: "smooth" });
  }, [current?.log.length]);
  useEffect(() => {
    if (!running) return;
    const t = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(t);
  }, [running]);

  const start = useCallback(async () => {
    if (running || !request.trim()) return;
    setCurrent(empty(actor, model, request));
    setSelectedRun(null);
    setReport(null);
    setFile(null);
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

  const openRun = useCallback(
    (id: string) => {
      hcxRunDetail(id).then(
        (rec) => {
          let state = empty(rec.actor, rec.model, rec.question);
          for (const ev of rec.events) state = reduce(state, ev);
          setCurrent(state);
          setSelectedRun(id);
          setActor(rec.actor);
          setModel(rec.model);
          setRequest(rec.question);
          setPane("report");
          setFile(null);
          showReport(rec.actor, state.check?.report ?? null);
        },
        (err) => notify(messageOf(err), "error"),
      );
    },
    [notify, showReport],
  );

  const fresh = () => {
    if (running) return;
    setCurrent(null);
    setSelectedRun(null);
    setReport(null);
    setFile(null);
    setRequest(config?.default_question ?? "");
  };

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

  const shown = useMemo(() => tree.filter((n) => !n.name.endsWith(".acl.json")), [tree]);
  const files = shown.filter((n) => n.kind === "file");
  // Folders fold; the output folder starts folded, since its files are what runs leave behind.
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({ 산출물: true });
  const toggleDir = (path: string) => setCollapsed((c) => ({ ...c, [path]: !c[path] }));
  const visible = useMemo(() => {
    const out: HcxNode[] = [];
    let hide: string | null = null;
    for (const n of shown) {
      if (hide && n.path.startsWith(hide + "/")) continue;
      hide = null;
      out.push(n);
      if (n.kind === "dir" && (collapsed[n.path] || n.access === "locked")) hide = n.path;
    }
    return out;
  }, [shown, collapsed]);
  const readable = files.filter((n) => n.access === "open").length;
  const live = startedAt ? Math.max(0, Math.round((now - startedAt) / 1000)) : 0;
  const canRun = !running && request.trim().length > 0 && (config?.api_key_present ?? true);
  const recent = useMemo(() => runs.slice(0, 6), [runs]);

  return (
    <>
      <aside className="run-side">
        <section className="runs">
          <h4>실행</h4>
          <button className={`run-row new ${current === null ? "on" : ""}`} onClick={fresh} disabled={running}>
            <PenIcon /> 새 실행
          </button>
          {running && current && (
            <div className="run-row on live">
              <span className="title">{current.log[0]?.kind === "request" ? current.log[0].text : ""}</span>
              <span className="meta">
                {current.actor} · {current.model} · 실행 중
              </span>
              <span className="dot" />
            </div>
          )}
          {runs.map((r) => (
            <button key={r.id} className={`run-row ${selectedRun === r.id ? "on" : ""}`} onClick={() => openRun(r.id)} disabled={running} title={r.question}>
              <span className="title">{r.question}</span>
              <span className="meta">
                {r.actor} · {r.model} · {ago(r.started)}
              </span>
              {!r.ok && <span className="dot bad" />}
            </button>
          ))}
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
            {visible.map((n) => (
              <div
                key={n.path}
                className={`node ${n.kind} ${n.access} ${file?.path === n.path ? "selected" : ""}`}
                style={{ paddingLeft: 10 + n.depth * 14 }}
                title={n.path}
                onClick={() => (n.kind === "file" ? openFile(n.path) : toggleDir(n.path))}
              >
                <span className="glyph">
                  {n.kind === "dir" ? n.access === "locked" ? <LockIcon /> : <ChevronIcon open={!collapsed[n.path]} /> : n.access === "open" ? <FileIcon /> : <LockIcon />}
                </span>
                <span className="name">{n.name}</span>
                <span className="who">{view === "admin" ? n.readers : n.access === "inherited" ? "권한 승계" : n.access === "locked" ? `${n.readers} 전용` : ""}</span>
              </div>
            ))}
          </div>
        </section>
      </aside>

      <section className="run-main">
        <div className="composer">
          <div className="who-row">
            <div className="chips" role="radiogroup" aria-label="사용자">
              {(config?.actors ?? Object.keys(SCOPE)).map((a) => (
                <button key={a} role="radio" aria-checked={a === actor} className="chip-actor" disabled={running || selectedRun !== null} title={SCOPE[a] ?? ""} onClick={() => setActor(a)}>
                  <span className="avatar">{a[0]}</span>
                  {a}
                </button>
              ))}
            </div>
            <span className="spacer" />
            <div className="segmented mini" role="radiogroup" aria-label="모델">
              {(config?.models ?? ["HCX-007", "HCX-005"]).map((m) => (
                <button key={m} aria-pressed={m === model} disabled={running || selectedRun !== null} onClick={() => setModel(m)}>
                  {m}
                </button>
              ))}
            </div>
          </div>
          <textarea rows={2} value={request} disabled={running || selectedRun !== null} placeholder="요청" onChange={(e) => setRequest(e.target.value)} />
          <div className="bar">
            <span className={`status ${running ? "live" : ""}`}>
              {running && <span className="dot" />}
              {running ? `${fmtSeconds(live)} 작업 중` : current?.seconds != null ? `${fmtSeconds(current.seconds)} 작업` : config && !config.api_key_present ? "CLOVASTUDIO_API_KEY 없음" : ""}
            </span>
            <span className="spacer" />
            {selectedRun !== null ? (
              <button onClick={fresh}>새 실행</button>
            ) : (
              <button className="primary" disabled={!canRun} title="⌘↩" onClick={() => void start()}>
                실행
              </button>
            )}
          </div>
        </div>

        <div className="run-log" ref={logRef}>
          {current === null && (
            <div className="recent">
              {recent.length > 0 && <h3>최근 실행</h3>}
              {recent.length === 0 && <div className="empty">아직 실행이 없습니다.</div>}
              <div className="cards">
                {recent.map((r) => (
                  <RunCard key={r.id} run={r} onOpen={() => openRun(r.id)} />
                ))}
              </div>
            </div>
          )}
          {current &&
            current.log.map((e, i) => {
              switch (e.kind) {
                case "request":
                  return (
                    <div className="entry request" key={i}>
                      <span className="who">{current.actor}</span>
                      <p>{e.text}</p>
                    </div>
                  );
                case "step": {
                  const d = describe(e.step);
                  const pending = !e.step.result;
                  const denied = e.step.result?.denied ?? false;
                  return (
                    <details className={`entry step ${pending ? "pending" : denied ? "denied" : "done"}`} key={i}>
                      <summary>
                        <span className="glyph">{denied ? <LockIcon /> : d.icon}</span>
                        <span className="label">{pending ? d.doing : d.done}</span>
                        {denied && <span className="why">{deniedReason(e.step)}</span>}
                        <span className="spacer" />
                        {pending ? <span className="wait" /> : denied ? null : <CheckIcon />}
                        <ChevronIcon open={false} />
                      </summary>
                      <div className="raw">
                        <div>
                          <code className="fn">{e.step.name}</code> <code>{JSON.stringify(e.step.args)}</code>
                        </div>
                        {e.step.result && <pre>{JSON.stringify(e.step.result.value, null, 2)}</pre>}
                      </div>
                    </details>
                  );
                }
                case "message":
                  return (
                    <div className="entry message" key={i}>
                      <span className="who">{current.model}</span>
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
            감사 로그{current && current.audit.length > 0 && <span className="count">{current.audit.length}</span>}
          </button>
          <button role="tab" aria-pressed={pane === "check"} onClick={() => setPane("check")}>
            권한 대조{current?.check && current.check.denied.length > 0 && <span className="count">{current.check.denied.length}</span>}
          </button>
          {file && (
            <button role="tab" aria-pressed={pane === "file"} onClick={() => setPane("file")}>
              파일
            </button>
          )}
        </div>

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
            {(!current || current.audit.length === 0) && <div className="empty">기록이 없습니다.</div>}
            {current && current.audit.length > 0 && (
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
                  {current.audit.map((e, i) => (
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
            {!current?.check && <div className="empty">실행이 끝나면 거절된 자료가 보고서에 명시되었는지 대조합니다.</div>}
            {current?.check && (
              <>
                <p className="lead">트리가 거절한 자료가 보고서에 적혀 있는지 경로로 확인한 결과입니다.</p>
                {!current.check.report && (
                  <div className="check-row no">
                    <CrossIcon /> 저장된 보고서가 없습니다.
                  </div>
                )}
                {current.check.denied.length === 0 && <div className="check-row">거절된 자료가 없습니다.</div>}
                {current.check.denied.map((d) => (
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
      </aside>
    </>
  );
}

/** A past run as a card: when, what, the first steps, and the report it left. */
function RunCard({ run, onOpen }: { run: HcxRunSummary; onOpen: () => void }) {
  return (
    <button className="run-card" onClick={onOpen}>
      <span className="when">{run.ok ? ago(run.started) : `실패 · ${ago(run.started)}`}</span>
      <span className="title">{run.question}</span>
      <span className="row">
        <span className="avatar">{run.actor[0]}</span>
        {run.actor} · {run.model}
      </span>
      <span className="row">
        {run.report ? (
          <>
            <DocIcon /> {leaf(run.report)}
          </>
        ) : (
          <>
            <CrossIcon /> 보고서 없음
          </>
        )}
      </span>
    </button>
  );
}

function ago(iso: string): string {
  const d = (Date.now() - new Date(iso).getTime()) / 1000;
  if (d < 60) return "방금";
  if (d < 3600) return `${Math.floor(d / 60)}분 전`;
  if (d < 86400) return `${Math.floor(d / 3600)}시간 전`;
  return `${Math.floor(d / 86400)}일 전`;
}

function fmtSeconds(s: number): string {
  const n = Math.round(s);
  return n >= 60 ? `${Math.floor(n / 60)}분 ${n % 60}초` : `${n}초`;
}

function AclLine({ text }: { text: string }) {
  const readers = /열람 권한: ([^(—\n]+)/.exec(text)?.[1]?.trim();
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

/** Enough Markdown for a report: headings, lists, tables, bold, inline code, and file paths kept whole. */
function Markdown({ text }: { text: string }) {
  const lines = text.split("\n");
  const out: ReactElement[] = [];
  const PATH = /((?:[가-힣A-Za-z0-9_.-]+\/)+[가-힣A-Za-z0-9_.-]+\.(?:md|csv|json|txt|jsonl|sqlite))/;
  const inline = (s: string) =>
    s.split(/(\*\*[^*]+\*\*|`[^`]+`)/).flatMap((part, k) => {
      if (part.startsWith("**")) return [<strong key={k}>{part.slice(2, -2)}</strong>];
      if (part.startsWith("`")) return [<code key={k}>{part.slice(1, -1)}</code>];
      return part.split(PATH).map((piece, j) =>
        PATH.test(piece) ? (
          <code className="path" key={`${k}-${j}`}>
            {piece}
          </code>
        ) : (
          piece
        ),
      );
    });
  // Lists, ordered or not, nested by indent: one block per run of list lines.
  const item = /^(\s*)(?:(\d+)\.|[-*•])\s+(.*)$/;
  type Li = { ordered: boolean; text: string; children: Li[] };
  const renderList = (items: Li[], key: number): ReactElement => {
    const ordered = items[0]?.ordered ?? false;
    const body = items.map((it, k) => (
      <li key={k}>
        {inline(it.text)}
        {it.children.length > 0 && renderList(it.children, k)}
      </li>
    ));
    return ordered ? <ol key={key}>{body}</ol> : <ul key={key}>{body}</ul>;
  };
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
    if (item.test(line)) {
      const root: Li[] = [];
      const stack: { indent: number; list: Li[] }[] = [{ indent: -1, list: root }];
      // A blank line inside a list does not end it when a list item follows.
      const nextIsItem = (j: number) => {
        while (j < lines.length && lines[j].trim() === "") j++;
        return j < lines.length && item.test(lines[j]);
      };
      while (i < lines.length && (item.test(lines[i]) || (lines[i].trim() === "" && nextIsItem(i)))) {
        if (lines[i].trim() === "") {
          i++;
          continue;
        }
        const m = item.exec(lines[i])!;
        const indent = m[1].length;
        while (stack.length > 1 && indent <= stack[stack.length - 1].indent) stack.pop();
        const li: Li = { ordered: m[2] !== undefined, text: m[3], children: [] };
        stack[stack.length - 1].list.push(li);
        stack.push({ indent, list: li.children });
        i++;
      }
      out.push(renderList(root, out.length));
      continue;
    }
    if (line.trim() !== "") out.push(<p key={out.length}>{inline(line)}</p>);
    i++;
  }
  return <div className="md">{out}</div>;
}
