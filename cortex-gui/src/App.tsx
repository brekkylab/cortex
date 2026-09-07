// The shell: two tabs, the state they share, and one status line.
//
// Both tabs stay mounted and the inactive one is hidden, rather than being unmounted on a
// switch. That is what keeps an expanded tree and a half-written system message across a click
// on the other tab — and the workspace tab's drag-and-drop listener is registered once for the
// life of the window instead of being torn down and rebuilt every time.

import { useCallback, useEffect, useState } from "react";

import { listAgents, listMounts, listResources, messageOf } from "./api";
import AgentTab from "./components/AgentTab";
import WorkspaceTab from "./components/WorkspaceTab";
import type { Agent, MountInfo, Resource } from "./types";

type Tab = "workspace" | "agents";
type Status = { text: string; tone: "ok" | "error" | "" };

export default function App() {
  const [tab, setTab] = useState<Tab>("workspace");
  const [mounts, setMounts] = useState<MountInfo[]>([]);
  const [resources, setResources] = useState<Resource[]>([]);
  const [agents, setAgents] = useState<Agent[]>([]);
  const [status, setStatus] = useState<Status>({ text: "빈 워크스페이스로 시작했습니다", tone: "" });

  const notify = useCallback((text: string, tone?: "ok" | "error") => {
    setStatus({ text, tone: tone ?? "" });
  }, []);

  const refreshMounts = useCallback(() => {
    listMounts().then(setMounts, (err) => notify(messageOf(err), "error"));
  }, [notify]);
  const refreshResources = useCallback(() => {
    listResources().then(setResources, (err) => notify(messageOf(err), "error"));
  }, [notify]);
  const refreshAgents = useCallback(() => {
    listAgents().then(setAgents, (err) => notify(messageOf(err), "error"));
  }, [notify]);

  useEffect(() => {
    refreshMounts();
    refreshResources();
    refreshAgents();
  }, [refreshMounts, refreshResources, refreshAgents]);

  return (
    <div className="app">
      <header className="titlebar">
        <div className="brand">
          Cortex
          <small>워크스페이스 · 메모리 상주 · 저장 없음</small>
        </div>
        <div className="tabs" role="tablist">
          <button role="tab" aria-selected={tab === "workspace"} onClick={() => setTab("workspace")}>
            워크스페이스
          </button>
          <button role="tab" aria-selected={tab === "agents"} onClick={() => setTab("agents")}>
            에이전트
          </button>
        </div>
        <span className="spacer" />
        <button disabled title="세션을 파일에서 여는 기능은 아직 없습니다">
          열기
        </button>
        <button disabled title="세션을 파일로 저장하는 기능은 아직 없습니다">
          저장
        </button>
      </header>

      <div className="body" style={{ display: tab === "workspace" ? "flex" : "none" }}>
        <WorkspaceTab
          active={tab === "workspace"}
          mounts={mounts}
          resources={resources}
          refreshMounts={refreshMounts}
          refreshResources={refreshResources}
          notify={notify}
        />
      </div>
      <div className="body" style={{ display: tab === "agents" ? "flex" : "none" }}>
        <AgentTab
          active={tab === "agents"}
          agents={agents}
          resources={resources}
          mounts={mounts}
          refreshAgents={refreshAgents}
          notify={notify}
        />
      </div>

      <footer className="statusbar">
        <span>마운트 {mounts.length}</span>
        <span>·</span>
        <span>리소스 {resources.length}</span>
        <span>·</span>
        <span>에이전트 {agents.length}</span>
        <span className="spacer" />
        <span className={`msg ${status.tone}`}>{status.text}</span>
      </footer>
    </div>
  );
}
