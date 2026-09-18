// The shell: three tabs, the state they share, and one status line.
//
// All tabs stay mounted and the inactive ones are hidden, so an expanded tree or a half-written
// request survives a switch, and the workspace tab's drag-and-drop listener is registered once.

import { useCallback, useEffect, useState } from "react";

import { listAgents, listMounts, listResources, messageOf } from "./api";
import AgentTab from "./components/AgentTab";
import HyperclovaTab from "./components/HyperclovaTab";
import WorkspaceTab from "./components/WorkspaceTab";
import type { Agent, MountInfo, Resource } from "./types";

type Tab = "run" | "workspace" | "agents";
type Theme = "system" | "light" | "dark";
const THEME_LABEL: Record<Theme, string> = { system: "시스템", light: "라이트", dark: "다크" };
const NEXT_THEME: Record<Theme, Theme> = { system: "light", light: "dark", dark: "system" };
type Status = { text: string; tone: "ok" | "error" | "" };

export default function App() {
  const [tab, setTab] = useState<Tab>("run");
  const [mounts, setMounts] = useState<MountInfo[]>([]);
  const [resources, setResources] = useState<Resource[]>([]);
  const [agents, setAgents] = useState<Agent[]>([]);
  const [status, setStatus] = useState<Status>({ text: "", tone: "" });
  const [presetModel, setPresetModel] = useState<string | null>(null);
  const [theme, setTheme] = useState<Theme>(() => {
    try {
      const saved = localStorage.getItem("theme");
      return saved === "light" || saved === "dark" ? saved : "system";
    } catch {
      return "system";
    }
  });
  useEffect(() => {
    const root = document.documentElement;
    if (theme === "system") delete root.dataset.theme;
    else root.dataset.theme = theme;
    try {
      localStorage.setItem("theme", theme);
    } catch {
      /* per-viewer convenience only */
    }
  }, [theme]);

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

  // The workspace is mounted by the Rust side a moment after launch.
  useEffect(() => {
    refreshResources();
    refreshAgents();
    let tries = 0;
    const timer = setInterval(() => {
      listMounts().then((m) => {
        setMounts(m);
        if (m.length > 1 || ++tries > 20) clearInterval(timer);
      });
    }, 250);
    return () => clearInterval(timer);
  }, [refreshResources, refreshAgents]);

  const runAgent = useCallback((agent: Agent) => {
    setPresetModel(agent.model);
    setTab("run");
  }, []);

  const onRunFinished = useCallback(() => {
    refreshMounts();
    notify("실행 완료", "ok");
  }, [refreshMounts, notify]);

  return (
    <div className="app">
      <header className="titlebar">
        <div className="brand">
          Cortex
          <small>for HyperCLOVA X</small>
        </div>
        <div className="tabs" role="tablist">
          <button role="tab" aria-selected={tab === "run"} onClick={() => setTab("run")}>
            실행
          </button>
          <button role="tab" aria-selected={tab === "workspace"} onClick={() => setTab("workspace")}>
            워크스페이스
          </button>
          <button role="tab" aria-selected={tab === "agents"} onClick={() => setTab("agents")}>
            에이전트
          </button>
        </div>
        <span className="spacer" />
        <button className="ghost theme" title="테마" onClick={() => setTheme(NEXT_THEME[theme])}>
          {THEME_LABEL[theme]}
        </button>
        <span className="session" title="이 창은 관리자 세션입니다. 실행은 실행 탭에서 고른 사용자의 권한으로 이루어집니다.">
          관리자 세션
        </span>
      </header>

      <div className="body" style={{ display: tab === "run" ? "flex" : "none" }}>
        <HyperclovaTab active={tab === "run"} notify={notify} onFinished={onRunFinished} presetModel={presetModel} />
      </div>
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
        <AgentTab active={tab === "agents"} agents={agents} resources={resources} mounts={mounts} refreshAgents={refreshAgents} notify={notify} onRun={runAgent} />
      </div>

      <footer className="statusbar">
        <span>연결 {Math.max(mounts.length - 1, 0)}</span>
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
