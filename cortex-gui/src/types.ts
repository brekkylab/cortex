// The shapes the Rust side sends. Mirrors of `src-tauri/src/state.rs` and `fsops.rs`, kept by
// hand: one crate and one bundle is not enough surface to be worth generating.

export type MountKind = "scratch" | "local" | "notion" | "s3";

export interface MountInfo {
  path: string;
  kind: MountKind;
  label: string;
  detail: string;
  writable: boolean;
}

export interface Entry {
  name: string;
  path: string;
  kind: "dir" | "file";
  /** Absent when the listing did not come with metadata — a store's choice, not a failure. */
  size: number | null;
  mtime_ms: number | null;
}

export interface FileContent {
  path: string;
  /** `null` for bytes that are not text. */
  text: string | null;
  size: number;
  truncated: boolean;
}

export interface ImportReport {
  files: number;
  bytes: number;
  skipped: string[];
}

export type ResourceKind = "memory" | "docset";

export interface Resource {
  /** The store's path in the workspace, which is also its identity. */
  id: string;
  kind: ResourceKind;
  /** The file's name without `.sqlite` — the name as it was typed. */
  name: string;
  created_ms: number;
}

export interface Agent {
  id: string;
  name: string;
  model: string;
  system_message: string;
  resource_ids: string[];
  paths: string[];
  created_ms: number;
}

export interface AgentSpec {
  name: string;
  model: string;
  system_message: string;
  resource_ids: string[];
  paths: string[];
}

export interface S3Form {
  bucket: string;
  region: string;
  access_key_id: string;
  secret_access_key: string;
  endpoint: string | null;
  key_prefix: string | null;
}

// ── HyperCLOVA X over the tree ── mirrors of `cortex_agent_hyperclova::session`.

export interface HcxConfig {
  workspace: string;
  actors: string[];
  models: string[];
  default_question: string;
  /** `bucket[/prefix]` serving one department, or null when every mount is a local folder. */
  s3: string | null;
  api_key_present: boolean;
  mem_present: boolean;
  /** Open the most recent run when the window opens. */
  open_latest: boolean;
}

export interface HcxNode {
  path: string;
  name: string;
  kind: "dir" | "file";
  depth: number;
  access: "open" | "locked" | "inherited";
  label: string;
  readers: string;
}

export interface HcxCall {
  name: string;
  arguments: unknown;
}

export interface HcxAuditEntry {
  at: string;
  actor: string;
  tool: string;
  path: string;
  allowed: boolean;
  detail: string;
}

export type HcxEvent =
  | { kind: "started"; actor: string; model: string; question: string; mounts: { name: string; source: string }[] }
  | { kind: "assistant"; text: string | null; calls: HcxCall[] }
  | { kind: "tool_result"; value: unknown; denied: boolean }
  | { kind: "notice"; text: string }
  | { kind: "audit"; entry: HcxAuditEntry }
  | { kind: "check"; report: string | null; denied: { path: string; mentioned: boolean }[] }
  | { kind: "finished"; seconds: number; log: string }
  | { kind: "failed"; message: string };

/** A recorded run as the history list shows it — `session::Summary`. */
export interface HcxRunSummary {
  id: string;
  actor: string;
  model: string;
  question: string;
  started: string;
  finished: string | null;
  ok: boolean;
  report: string | null;
  seconds: number | null;
}

/** A recorded run whole — `session::Record`. */
export interface HcxRunRecord extends Omit<HcxRunSummary, "report" | "seconds"> {
  events: HcxEvent[];
}
