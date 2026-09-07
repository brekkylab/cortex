// The workspace tab: what the session can see, and everything that changes it.
//
// Three panes, left to right. The sidebar is the *stores* — what is mounted where, and the
// buttons that mount more — plus the memories and docsets registered against them. The tree is
// the workspace itself, read back from the Rust side one directory at a time: a directory is
// listed when it is first opened and re-listed whenever something writes into it, which is what
// keeps a Notion mount showing what Notion says rather than what it said at connect time. The
// editor is the selected file.
//
// Nothing here caches a file. `fs_read` on select and `fs_write` on save is the whole of it, so
// two windows onto the same store cannot disagree about which of them is right.

import { type ReactNode, useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";

import {
  fsDelete,
  fsImport,
  fsList,
  fsMkdir,
  fsRead,
  fsRename,
  fsTouch,
  fsWrite,
  messageOf,
  mountLocal,
  removeResource,
  unmount,
} from "../api";
import type { Entry, FileContent, MountInfo, Resource } from "../types";
import { NotionDialog, PromptDialog, ResourceDialog, S3Dialog } from "./dialogs";
import { FileIcon, FolderIcon } from "./icons";

type Dialog =
  | { kind: "notion" | "s3" | "mkdir" | "touch" | "rename" }
  | { kind: "resource"; resource: "memory" | "docset" };

interface Props {
  /** Whether this tab is the one on screen. Drops are ignored while it is not. */
  active: boolean;
  mounts: MountInfo[];
  resources: Resource[];
  refreshMounts: () => void;
  refreshResources: () => void;
  notify: (text: string, tone?: "ok" | "error") => void;
}

const ROOT = "/";

export default function WorkspaceTab(props: Props) {
  const { notify, refreshMounts, refreshResources } = props;

  const [children, setChildren] = useState<Record<string, Entry[]>>({});
  const [expanded, setExpanded] = useState<Record<string, boolean>>({ [ROOT]: true });
  const [selected, setSelected] = useState<Entry>({
    name: "workspace",
    path: ROOT,
    kind: "dir",
    size: null,
    mtime_ms: null,
  });
  const [file, setFile] = useState<FileContent | null>(null);
  const [draft, setDraft] = useState("");
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [dragging, setDragging] = useState(false);

  /** The directory a create, an import or a drop lands in. */
  const targetDir = selected.kind === "dir" ? selected.path : parentOf(selected.path);
  const dirty = file !== null && file.text !== null && draft !== file.text;
  const writable = isWritable(props.mounts, selected.path);

  const loadDir = useCallback(
    async (path: string) => {
      try {
        const entries = await fsList(path);
        setChildren((prev) => ({ ...prev, [path]: entries }));
      } catch (err) {
        notify(messageOf(err), "error");
      }
    },
    [notify],
  );

  useEffect(() => {
    void loadDir(ROOT);
  }, [loadDir]);

  // What the tree shows can change without the tree being touched: a mount added or removed
  // changes what the root lists, and a memory or docset is a *file in the workspace*, so
  // registering one creates a directory and a file under `/.cortex`. Re-list the root, and any
  // resource directory already open, whenever either list changes.
  const childrenRef = useRef(children);
  childrenRef.current = children;
  useEffect(() => {
    const open = Object.keys(childrenRef.current).filter((dir) => dir.startsWith("/.cortex"));
    for (const dir of new Set([ROOT, ...open])) void loadDir(dir);
  }, [loadDir, props.mounts, props.resources]);

  const importInto = useCallback(
    async (dest: string, sources: string[]) => {
      if (sources.length === 0) return;
      try {
        const report = await fsImport(dest, sources);
        setExpanded((prev) => ({ ...prev, [dest]: true }));
        await loadDir(dest);
        const failed = report.skipped.length;
        notify(
          `${dest} 에 파일 ${report.files}개 (${bytes(report.bytes)})` +
            (failed ? ` · 건너뜀 ${failed}개: ${report.skipped[0]}` : ""),
          failed ? "error" : "ok",
        );
      } catch (err) {
        notify(messageOf(err), "error");
      }
    },
    [loadDir, notify],
  );

  // The drop target and whether the tab is on screen, read at drop time rather than closed over:
  // the listener is registered once, and a listener re-registered on every selection would drop
  // events in the window between the two.
  const dropContext = useRef({ dir: targetDir, active: props.active });
  dropContext.current = { dir: targetDir, active: props.active };

  useEffect(() => {
    let stop: (() => void) | undefined;
    void getCurrentWebview()
      .onDragDropEvent((event) => {
        if (!dropContext.current.active) return;
        if (event.payload.type === "over") setDragging(true);
        else if (event.payload.type === "drop") {
          setDragging(false);
          void importInto(dropContext.current.dir, event.payload.paths);
        } else setDragging(false);
      })
      .then((unlisten) => {
        stop = unlisten;
      });
    return () => stop?.();
  }, [importInto]);

  const pick = async (entry: Entry) => {
    setSelected(entry);
    if (entry.kind === "dir") {
      setFile(null);
      if (!children[entry.path]) void loadDir(entry.path);
      setExpanded((prev) => ({ ...prev, [entry.path]: !prev[entry.path] }));
      return;
    }
    try {
      const content = await fsRead(entry.path);
      setFile(content);
      setDraft(content.text ?? "");
    } catch (err) {
      setFile(null);
      notify(messageOf(err), "error");
    }
  };

  const save = async () => {
    if (!file) return;
    try {
      await fsWrite(file.path, draft);
      setFile({ ...file, text: draft, size: new Blob([draft]).size });
      await loadDir(parentOf(file.path));
      notify(`${file.path} 저장됨`, "ok");
    } catch (err) {
      notify(messageOf(err), "error");
    }
  };

  const remove = async () => {
    if (selected.path === ROOT) return;
    try {
      await fsDelete(selected.path);
      const parent = parentOf(selected.path);
      await loadDir(parent);
      setFile(null);
      setSelected({ name: parent, path: parent, kind: "dir", size: null, mtime_ms: null });
      notify(`${selected.path} 삭제됨`, "ok");
    } catch (err) {
      notify(messageOf(err), "error");
    }
  };

  const addFiles = async () => {
    const picked = await open({ multiple: true, title: "워크스페이스에 추가할 파일" });
    if (picked) await importInto(targetDir, Array.isArray(picked) ? picked : [picked]);
  };

  const connectLocal = async () => {
    const picked = await open({ directory: true, title: "연결할 폴더" });
    if (typeof picked !== "string") return;
    const name = picked.split("/").filter(Boolean).pop() ?? "local";
    try {
      const mount = await mountLocal(`/${name}`, picked);
      refreshMounts();
      notify(`${mount.path} ← ${mount.detail}`, "ok");
    } catch (err) {
      notify(messageOf(err), "error");
    }
  };

  const rows = (dir: string, depth: number): ReactNode[] =>
    (children[dir] ?? []).flatMap((entry) => {
      const row = (
        <Row
          key={entry.path}
          entry={entry}
          depth={depth}
          open={!!expanded[entry.path]}
          selected={selected.path === entry.path}
          onPick={() => void pick(entry)}
        />
      );
      return entry.kind === "dir" && expanded[entry.path]
        ? [row, ...rows(entry.path, depth + 1)]
        : [row];
    });

  return (
    <>
      <aside className="sidebar">
        <div className="section-head">
          저장소
          <span className="spacer" />
          <button className="ghost" title="새로 고침" onClick={refreshMounts}>
            ↻
          </button>
        </div>
        {props.mounts.map((mount) => (
          <div className="mount" key={mount.path}>
            <div className="meta">
              <div className="name">
                <strong>{mount.label}</strong>
                <span className="badge">{mount.kind}</span>
                {!mount.writable && <span className="badge ro">읽기 전용</span>}
              </div>
              <div className="detail" title={mount.detail}>
                {mount.path} · {mount.detail}
              </div>
            </div>
            {mount.kind !== "scratch" && (
              <button
                className="ghost danger"
                title="연결 해제"
                onClick={async () => {
                  try {
                    await unmount(mount.path);
                    refreshMounts();
                    notify(`${mount.path} 연결 해제됨`, "ok");
                  } catch (err) {
                    notify(messageOf(err), "error");
                  }
                }}
              >
                ✕
              </button>
            )}
          </div>
        ))}
        <div className="actions">
          <button onClick={connectLocal}>폴더 연결</button>
          <button onClick={() => setDialog({ kind: "notion" })}>Notion 연결</button>
          <button onClick={() => setDialog({ kind: "s3" })}>S3 연결</button>
        </div>

        <div className="section-head">
          리소스
          <span className="spacer" />
          <button className="ghost" title="새로 고침" onClick={refreshResources}>
            ↻
          </button>
        </div>
        {props.resources.length === 0 && (
          <p className="empty">
            아직 없습니다. 여기서 만들면 워크스페이스 안에 저장소가 생기고, 에이전트 탭에서
            고를 수 있습니다.
          </p>
        )}
        {props.resources.map((resource) => (
          <div className="mount" key={resource.id}>
            <div className="meta">
              <div className="name">
                <strong>{resource.name}</strong>
                <span className={`badge kind-${resource.kind}`}>{resource.kind}</span>
              </div>
              <div className="detail" title={resource.id}>
                {resource.id}
              </div>
            </div>
            <button
              className="ghost danger"
              title="삭제"
              onClick={async () => {
                try {
                  await removeResource(resource.id);
                  refreshResources();
                  notify(`${resource.id} 삭제됨`, "ok");
                } catch (err) {
                  notify(messageOf(err), "error");
                }
              }}
            >
              ✕
            </button>
          </div>
        ))}
        <div className="actions">
          <button onClick={() => setDialog({ kind: "resource", resource: "memory" })}>
            + memory
          </button>
          <button onClick={() => setDialog({ kind: "resource", resource: "docset" })}>
            + docset
          </button>
        </div>
      </aside>

      <section className="tree-pane">
        <div className="pane-head">
          <button className="ghost" onClick={addFiles} title="파일 가져오기">
            파일 추가
          </button>
          <button className="ghost" onClick={() => setDialog({ kind: "mkdir" })}>
            새 폴더
          </button>
          <button className="ghost" onClick={() => setDialog({ kind: "touch" })}>
            새 파일
          </button>
          <span className="spacer" />
          <button
            className="ghost"
            onClick={() => setDialog({ kind: "rename" })}
            disabled={selected.path === ROOT}
            title="이름 변경"
          >
            ✎
          </button>
          <button
            className="ghost danger"
            onClick={remove}
            disabled={selected.path === ROOT}
            title="삭제"
          >
            ✕
          </button>
          <button className="ghost" onClick={() => void loadDir(targetDir)} title="새로 고침">
            ↻
          </button>
        </div>
        <div className="tree">
          <Row
            entry={{ name: "workspace", path: ROOT, kind: "dir", size: null, mtime_ms: null }}
            depth={0}
            open
            selected={selected.path === ROOT}
            onPick={() =>
              setSelected({
                name: "workspace",
                path: ROOT,
                kind: "dir",
                size: null,
                mtime_ms: null,
              })
            }
          />
          {rows(ROOT, 1)}
        </div>
      </section>

      <section className="editor-pane">
        <div className="pane-head">
          <span className="path">{file ? file.path : targetDir}</span>
          <span className="spacer" />
          {file && (
            <>
              <span style={{ color: "var(--text-faint)" }}>{bytes(file.size)}</span>
              <button
                className="primary"
                onClick={save}
                disabled={!dirty || !writable || file.text === null}
                title={writable ? "" : "읽기 전용 저장소입니다"}
              >
                저장
              </button>
            </>
          )}
        </div>
        {file?.truncated && (
          <div className="notice">
            파일이 커서 앞부분 1 MiB만 열었습니다 — 저장하면 나머지가 사라지므로 편집이 막혀 있습니다.
          </div>
        )}
        {file === null ? (
          <div className="placeholder">
            <div className="big">＋</div>
            <div>
              파일을 이 창에 끌어다 놓거나, <strong>파일 추가</strong> 로 가져옵니다.
              <br />
              놓는 위치는 지금 선택된 폴더 — <code>{targetDir}</code> 입니다.
            </div>
          </div>
        ) : file.text === null ? (
          <div className="placeholder">
            <div className="big">◻</div>
            <div>텍스트가 아닌 파일입니다.</div>
          </div>
        ) : (
          <textarea
            value={draft}
            spellCheck={false}
            readOnly={!writable || file.truncated}
            onChange={(event) => setDraft(event.target.value)}
          />
        )}
      </section>

      {dragging && <div className="dropzone">{targetDir} 에 놓기</div>}

      {dialog?.kind === "notion" && (
        <NotionDialog onDone={refreshMounts} onClose={() => setDialog(null)} />
      )}
      {dialog?.kind === "s3" && <S3Dialog onDone={refreshMounts} onClose={() => setDialog(null)} />}
      {dialog?.kind === "resource" && (
        <ResourceDialog
          kind={dialog.resource}
          onDone={refreshResources}
          onClose={() => setDialog(null)}
        />
      )}
      {dialog?.kind === "mkdir" && (
        <PromptDialog
          title="새 폴더"
          hint={`${targetDir} 아래에 만듭니다.`}
          label="이름"
          submitLabel="만들기"
          onClose={() => setDialog(null)}
          onSubmit={async (name) => {
            await fsMkdir(joinPath(targetDir, name));
            setExpanded((prev) => ({ ...prev, [targetDir]: true }));
            await loadDir(targetDir);
          }}
        />
      )}
      {dialog?.kind === "touch" && (
        <PromptDialog
          title="새 파일"
          hint={`${targetDir} 아래에 빈 파일을 만듭니다.`}
          label="이름"
          placeholder="notes.md"
          submitLabel="만들기"
          onClose={() => setDialog(null)}
          onSubmit={async (name) => {
            await fsTouch(joinPath(targetDir, name));
            setExpanded((prev) => ({ ...prev, [targetDir]: true }));
            await loadDir(targetDir);
          }}
        />
      )}
      {dialog?.kind === "rename" && (
        <PromptDialog
          title="이름 변경"
          hint={selected.path}
          label="새 이름"
          submitLabel="변경"
          onClose={() => setDialog(null)}
          onSubmit={async (name) => {
            const parent = parentOf(selected.path);
            await fsRename(selected.path, joinPath(parent, name));
            await loadDir(parent);
            setFile(null);
          }}
        />
      )}
    </>
  );
}

function Row(props: {
  entry: Entry;
  depth: number;
  open: boolean;
  selected: boolean;
  onPick: () => void;
}) {
  const { entry } = props;
  return (
    <div
      className={`row${props.selected ? " selected" : ""}`}
      style={{ paddingLeft: 8 + props.depth * 13 }}
      onClick={props.onPick}
    >
      <span className="twisty">{entry.kind === "dir" ? (props.open ? "▾" : "▸") : ""}</span>
      {entry.kind === "dir" ? <FolderIcon open={props.open} /> : <FileIcon />}
      <span className="label">{entry.name}</span>
      {entry.kind === "file" && entry.size !== null && (
        <span className="size">{bytes(entry.size)}</span>
      )}
    </div>
  );
}

/** The mount that owns `path`, by longest prefix — the same rule `WorkFs` routes by. */
function isWritable(mounts: MountInfo[], path: string): boolean {
  let owner: MountInfo | undefined;
  for (const mount of mounts) {
    const prefix = mount.path === "/" ? "/" : `${mount.path}/`;
    if (path === mount.path || path.startsWith(prefix)) {
      if (!owner || mount.path.length > owner.path.length) owner = mount;
    }
  }
  return owner?.writable ?? false;
}

function parentOf(path: string): string {
  const cut = path.lastIndexOf("/");
  return cut <= 0 ? ROOT : path.slice(0, cut);
}

function joinPath(dir: string, name: string): string {
  return `${dir === ROOT ? "" : dir}/${name}`;
}

function bytes(size: number): string {
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KiB`;
  return `${(size / 1024 / 1024).toFixed(1)} MiB`;
}
