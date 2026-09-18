// The agent tab: what an agent is given — a model, a system message, the memories, docsets and
// paths it may read — kept as a record and run from the run screen with that model.

import { useCallback, useEffect, useState } from "react";

import { createAgent, deleteAgent, fsList, messageOf } from "../api";
import type { Agent, Entry, MountInfo, Resource } from "../types";

interface Props {
  active: boolean;
  agents: Agent[];
  resources: Resource[];
  mounts: MountInfo[];
  refreshAgents: () => void;
  notify: (text: string, tone?: "ok" | "error") => void;
  onRun: (agent: Agent) => void;
}

const MODELS: { group: string; items: string[] }[] = [
  { group: "API", items: ["HCX-007", "HCX-005", "HCX-DASH-002"] },
  {
    group: "로컬 AI",
    items: [
      "naver-hyperclovax/HyperCLOVAX-SEED-Think-14B",
      "Qwen/Qwen3.8-27B",
      "Qwen/Qwen3.5-32B",
      "Qwen/Qwen3.5-14B",
      "Qwen/Qwen3.5-8B",
      "Qwen/Qwen3.5-4B",
      "google/gemma-4-31B-it",
      "google/gemma-4-26B-A4B-it",
      "google/gemma-4-12B-it",
      "google/gemma-4-E4B-it",
    ],
  },
];
const runsOn = (model: string) => (model.startsWith("HCX") ? "API" : "로컬 AI");

const BLANK = { name: "", model: "HCX-007", system_message: "" };

export default function AgentTab(props: Props) {
  const { notify, refreshAgents } = props;

  const [form, setForm] = useState(BLANK);
  const [resourceIds, setResourceIds] = useState<string[]>([]);
  const [paths, setPaths] = useState<string[]>([]);
  const [roots, setRoots] = useState<Entry[]>([]);
  const [busy, setBusy] = useState(false);

  const loadRoots = useCallback(async () => {
    try {
      const entries = await fsList("/");
      setRoots(entries.filter((entry) => !entry.name.startsWith(".")));
    } catch (err) {
      notify(messageOf(err), "error");
    }
  }, [notify]);

  useEffect(() => {
    if (props.active) void loadRoots();
  }, [props.active, loadRoots, props.mounts]);

  const toggle = (list: string[], value: string) =>
    list.includes(value) ? list.filter((item) => item !== value) : [...list, value];

  const submit = async () => {
    setBusy(true);
    try {
      const agent = await createAgent({ ...form, resource_ids: resourceIds, paths });
      refreshAgents();
      setForm(BLANK);
      setResourceIds([]);
      setPaths([]);
      notify(`${agent.name} 등록`, "ok");
    } catch (err) {
      notify(messageOf(err), "error");
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <aside className="agent-list">
        <div className="section-head">
          에이전트
          <span className="spacer" />
          <button className="ghost" title="새로 고침" onClick={refreshAgents}>
            ↻
          </button>
        </div>
        {props.agents.length === 0 && <p className="empty">등록된 에이전트가 없습니다.</p>}
        {props.agents.map((agent) => (
          <div className="agent-card" key={agent.id}>
            <div className="name">
              <strong>{agent.name}</strong>
              <span className="badge">{agent.model}</span>
              <span className="badge ro">{runsOn(agent.model)}</span>
              <span className="spacer" />
              <button className="ghost" title="이 모델로 실행 화면을 엽니다" onClick={() => props.onRun(agent)}>
                실행
              </button>
              <button
                className="ghost danger"
                title="삭제"
                onClick={async () => {
                  await deleteAgent(agent.id);
                  refreshAgents();
                }}
              >
                ✕
              </button>
            </div>
            <div className="sub" title={agent.system_message}>
              {agent.system_message}
            </div>
            <div className="sub">
              리소스 {agent.resource_ids.length} · 경로 {agent.paths.length}
            </div>
          </div>
        ))}
      </aside>

      <section className="form-pane">
        <h2>새 에이전트</h2>

        <div className="form-grid">
          <label>
            <span>이름</span>
            <input value={form.name} onChange={(event) => setForm({ ...form, name: event.target.value })} />
          </label>
          <label>
            <span>모델</span>
            <select value={form.model} onChange={(event) => setForm({ ...form, model: event.target.value })}>
              {MODELS.map((g) => (
                <optgroup key={g.group} label={g.group}>
                  {g.items.map((m) => (
                    <option key={m} value={m}>
                      {m}
                    </option>
                  ))}
                </optgroup>
              ))}
            </select>
          </label>

          <label className="wide">
            <span>시스템 메시지</span>
            <textarea rows={6} value={form.system_message} onChange={(event) => setForm({ ...form, system_message: event.target.value })} />
          </label>

          <label className="wide">
            <span>memory · docset</span>
            <div className="picker">
              {props.resources.length === 0 && (
                <div className="pick">
                  <span style={{ color: "var(--text-faint)" }}>워크스페이스 탭에서 memory 또는 docset 을 먼저 만듭니다.</span>
                </div>
              )}
              {props.resources.map((resource) => (
                <label className="pick" key={resource.id}>
                  <input type="checkbox" checked={resourceIds.includes(resource.id)} onChange={() => setResourceIds(toggle(resourceIds, resource.id))} />
                  <span>{resource.name}</span>
                  <span className={`badge kind-${resource.kind}`}>{resource.kind}</span>
                  <span className="sub">{resource.id}</span>
                </label>
              ))}
            </div>
          </label>

          <label className="wide">
            <span>읽을 경로</span>
            <div className="picker">
              {roots.length === 0 && (
                <div className="pick">
                  <span style={{ color: "var(--text-faint)" }}>워크스페이스가 비어 있습니다.</span>
                </div>
              )}
              {roots.map((entry) => (
                <label className="pick" key={entry.path}>
                  <input type="checkbox" checked={paths.includes(entry.path)} onChange={() => setPaths(toggle(paths, entry.path))} />
                  <span>{entry.name}</span>
                  <span className="badge">{entry.kind === "dir" ? "폴더" : "파일"}</span>
                  <span className="sub">{entry.path}</span>
                </label>
              ))}
            </div>
          </label>

          <div className="wide" style={{ display: "flex", gap: 8, marginTop: 6 }}>
            <button className="primary" disabled={busy || !form.name.trim() || !form.system_message.trim()} onClick={submit}>
              등록
            </button>
          </div>
        </div>
      </section>
    </>
  );
}
