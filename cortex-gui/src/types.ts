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
  id: string;
  kind: ResourceKind;
  name: string;
  note: string;
  /** Where its store will live once `mem` / `index` are wired in. */
  store_path: string;
  /** Always false today: nothing is written at `store_path` yet. */
  backed: boolean;
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
