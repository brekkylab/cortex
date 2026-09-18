// Every call into the Rust side, in one place and typed once.
//
// `invoke` is untyped at the boundary, so a command name misspelled in a component fails at
// runtime with a message about an unknown command. Naming each one here means a component
// cannot spell it at all.

import { invoke } from "@tauri-apps/api/core";

import type {
  Agent,
  AgentSpec,
  Entry,
  FileContent,
  ImportReport,
  MountInfo,
  Resource,
  ResourceKind,
  S3Form,
} from "./types";

export const fsList = (path: string) => invoke<Entry[]>("fs_list", { path });
export const fsRead = (path: string) => invoke<FileContent>("fs_read", { path });
export const fsWrite = (path: string, text: string) => invoke<void>("fs_write", { path, text });
export const fsTouch = (path: string) => invoke<void>("fs_touch", { path });
export const fsMkdir = (path: string) => invoke<void>("fs_mkdir", { path });
export const fsDelete = (path: string) => invoke<void>("fs_delete", { path });
export const fsRename = (from: string, to: string) => invoke<void>("fs_rename", { from, to });
export const fsImport = (dest: string, sources: string[]) =>
  invoke<ImportReport>("fs_import", { dest, sources });

export const listMounts = () => invoke<MountInfo[]>("mounts");
export const mountLocal = (path: string, hostRoot: string) =>
  invoke<MountInfo>("mount_local", { path, hostRoot });
export const mountNotion = (path: string, apiKey: string) =>
  invoke<MountInfo>("mount_notion", { path, apiKey });
export const mountS3 = (path: string, form: S3Form) => invoke<MountInfo>("mount_s3", { path, form });
export const unmount = (path: string) => invoke<void>("unmount", { path });

export const listResources = () => invoke<Resource[]>("resources");
export const addResource = (kind: ResourceKind, name: string) =>
  invoke<Resource>("add_resource", { form: { kind, name } });
export const removeResource = (id: string) => invoke<void>("remove_resource", { id });

export const listAgents = () => invoke<Agent[]>("agents");
export const createAgent = (spec: AgentSpec) => invoke<Agent>("create_agent", { spec });
export const deleteAgent = (id: string) => invoke<void>("delete_agent", { id });

/** A rejected command arrives as whatever the Rust side serialized — a string, here. */
export function messageOf(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  return String(err);
}

// ── HyperCLOVA X over the tree ──────────────────────────────────────────────

import type { HcxConfig, HcxEvent, HcxNode } from "./types";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export const hcxConfig = () => invoke<HcxConfig>("hcx_config");
export const hcxTree = (actor: string) => invoke<HcxNode[]>("hcx_tree", { actor });
/** A file as `actor` may read it — a denial arrives as the rejection message. */
export const hcxRead = (actor: string, path: string) => invoke<string>("hcx_read", { actor, path });
/** Starts a run; what happens arrives on `onHcxEvent`. Rejects while another run is going. */
export const hcxRun = (actor: string, model: string, question: string) =>
  invoke<void>("hcx_run", { actor, model, question });
export const onHcxEvent = (handler: (event: HcxEvent) => void): Promise<UnlistenFn> =>
  listen<HcxEvent>("hcx", (e) => handler(e.payload));
