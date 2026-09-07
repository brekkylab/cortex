// The agent tab: pick what an agent may read, write what it is, register it.
//
// Registering is where this stops. ailoy is what would run one, and until it is wired in the
// useful thing to get right is the record — a system message, a model, and the exact set of
// memories, docsets and workspace paths the agent was given — because that record is the input a
// runtime takes. So the form collects it, the list shows it, and the run button says plainly
// that it is not connected to anything yet rather than pretending to start something.

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
}

const MODELS = ["Qwen/Qwen3-0.6B", "Qwen/Qwen3-1.7B", "Qwen/Qwen3-8B"];

const BLANK = {
  name: "",
  model: MODELS[0],
  system_message: "",
};

export default function AgentTab(props: Props) {
  const { notify, refreshAgents } = props;

  const [form, setForm] = useState(BLANK);
  const [resourceIds, setResourceIds] = useState<string[]>([]);
  const [paths, setPaths] = useState<string[]>([]);
  const [roots, setRoots] = useState<Entry[]>([]);
  const [busy, setBusy] = useState(false);

  // The top level of the workspace, which is what the path picker offers. Re-read whenever the
  // tab comes forward: a directory created or a store connected in the other tab is exactly the
  // thing somebody switches over here to select.
  const loadRoots = useCallback(async () => {
    try {
      // Dot-entries dropped: `/.cortex` is where memories and docsets live, and those are picked
      // in the list above by name rather than as a directory to read.
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
      notify(`에이전트 '${agent.name}' 등록됨`, "ok");
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
          등록된 에이전트
          <span className="spacer" />
          <button className="ghost" title="새로 고침" onClick={refreshAgents}>
            ↻
          </button>
        </div>
        {props.agents.length === 0 && <p className="empty">아직 없습니다.</p>}
        {props.agents.map((agent) => (
          <div className="agent-card" key={agent.id}>
            <div className="name">
              <strong>{agent.name}</strong>
              <span className="badge">{agent.model || "모델 미지정"}</span>
              <span className="spacer" />
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
        <p className="lede">
          워크스페이스에서 읽을 것을 고르고 시스템 메시지를 쓰면 에이전트가 등록됩니다. 실행은
          아직 붙어 있지 않습니다 — ailoy 연동이 들어오면 이 기록이 그대로 런타임의 입력이 됩니다.
        </p>

        <div className="form-grid">
          <label>
            <span>이름</span>
            <input
              value={form.name}
              placeholder="문서 도우미"
              onChange={(event) => setForm({ ...form, name: event.target.value })}
            />
          </label>
          <label>
            <span>모델</span>
            <input
              list="agent-models"
              value={form.model}
              onChange={(event) => setForm({ ...form, model: event.target.value })}
            />
            <datalist id="agent-models">
              {MODELS.map((model) => (
                <option key={model} value={model} />
              ))}
            </datalist>
          </label>

          <label className="wide">
            <span>시스템 메시지</span>
            <textarea
              rows={7}
              value={form.system_message}
              placeholder="너는 이 워크스페이스의 문서를 근거로만 답한다. 근거가 없으면 없다고 말한다."
              onChange={(event) => setForm({ ...form, system_message: event.target.value })}
            />
          </label>

          <label className="wide">
            <span>memory · docset</span>
            <div className="picker">
              {props.resources.length === 0 && (
                <div className="pick">
                  <span style={{ color: "var(--text-faint)" }}>
                    워크스페이스 탭에서 + memory / + docset 으로 먼저 등록합니다.
                  </span>
                </div>
              )}
              {props.resources.map((resource) => (
                <label className="pick" key={resource.id}>
                  <input
                    type="checkbox"
                    checked={resourceIds.includes(resource.id)}
                    onChange={() => setResourceIds(toggle(resourceIds, resource.id))}
                  />
                  <span>{resource.name}</span>
                  <span className={`badge kind-${resource.kind}`}>{resource.kind}</span>
                  <span className="sub">{resource.id}</span>
                </label>
              ))}
            </div>
          </label>

          <label className="wide">
            <span>워크스페이스에서 읽을 경로</span>
            <div className="picker">
              {roots.length === 0 && (
                <div className="pick">
                  <span style={{ color: "var(--text-faint)" }}>
                    워크스페이스가 비어 있습니다. 파일을 올리거나 저장소를 연결해 주세요.
                  </span>
                </div>
              )}
              {roots.map((entry) => (
                <label className="pick" key={entry.path}>
                  <input
                    type="checkbox"
                    checked={paths.includes(entry.path)}
                    onChange={() => setPaths(toggle(paths, entry.path))}
                  />
                  <span>{entry.name}</span>
                  <span className="badge">{entry.kind}</span>
                  <span className="sub">{entry.path}</span>
                </label>
              ))}
            </div>
          </label>

          <div className="wide" style={{ display: "flex", gap: 8, marginTop: 6 }}>
            <button className="primary" disabled={busy} onClick={submit}>
              에이전트 등록
            </button>
            <button
              disabled
              title="ailoy 연동 후 활성화됩니다"
              onClick={(event) => event.preventDefault()}
            >
              실행 (준비 중)
            </button>
          </div>
        </div>
      </section>
    </>
  );
}
