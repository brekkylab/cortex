// The forms behind the workspace toolbar: two connectors, the memory/docset stub, and the
// one-field prompt that "새 폴더" and "새 파일" share.
//
// Each holds its own draft and its own error, and reports failure in place rather than through
// the status bar — a rejected connection is about the fields still on screen, and closing the
// dialog to read why would take them away.

import { useState } from "react";

import { addResource, messageOf, mountNotion, mountS3 } from "../api";
import type { MountInfo, Resource, ResourceKind } from "../types";
import Modal from "./Modal";

/** A single text field — a new directory, a new file. */
export function PromptDialog(props: {
  title: string;
  hint: string;
  label: string;
  placeholder?: string;
  submitLabel: string;
  onSubmit: (value: string) => Promise<void>;
  onClose: () => void;
}) {
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    setBusy(true);
    try {
      await props.onSubmit(value.trim());
      props.onClose();
    } catch (err) {
      setError(messageOf(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={props.title}
      hint={props.hint}
      error={error}
      submitLabel={props.submitLabel}
      busy={busy}
      onSubmit={submit}
      onClose={props.onClose}
    >
      <label>
        <span>{props.label}</span>
        <input
          autoFocus
          value={value}
          placeholder={props.placeholder}
          onChange={(event) => setValue(event.target.value)}
        />
      </label>
    </Modal>
  );
}

export function NotionDialog(props: {
  onDone: (mount: MountInfo) => void;
  onClose: () => void;
}) {
  const [path, setPath] = useState("/notion");
  const [apiKey, setApiKey] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      props.onDone(await mountNotion(path, apiKey));
      props.onClose();
    } catch (err) {
      setError(messageOf(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title="Notion 연결"
      hint="통합(integration) 토큰으로 워크스페이스를 읽기 전용으로 붙입니다. 페이지는 page.json 으로 렌더링되고, 하위 페이지는 디렉터리가 됩니다."
      error={error}
      submitLabel="연결"
      busy={busy}
      onSubmit={submit}
      onClose={props.onClose}
    >
      <label>
        <span>마운트 경로</span>
        <input value={path} onChange={(event) => setPath(event.target.value)} />
      </label>
      <label>
        <span>Integration token</span>
        <input
          autoFocus
          type="password"
          placeholder="ntn_…"
          value={apiKey}
          onChange={(event) => setApiKey(event.target.value)}
        />
      </label>
    </Modal>
  );
}

export function S3Dialog(props: { onDone: (mount: MountInfo) => void; onClose: () => void }) {
  const [path, setPath] = useState("/s3");
  const [form, setForm] = useState({
    bucket: "",
    region: "us-east-1",
    access_key_id: "",
    secret_access_key: "",
    endpoint: "",
    key_prefix: "",
  });
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const field = (key: keyof typeof form) => ({
    value: form[key],
    onChange: (event: { target: { value: string } }) =>
      setForm({ ...form, [key]: event.target.value }),
  });

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      props.onDone(
        await mountS3(path, {
          ...form,
          endpoint: form.endpoint || null,
          key_prefix: form.key_prefix || null,
        }),
      );
      props.onClose();
    } catch (err) {
      setError(messageOf(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title="S3 연결"
      hint="버킷의 키를 읽기 전용 트리로 붙입니다. 연결하기 전에 목록 요청을 한 번 보내 자격 증명을 확인합니다."
      error={error}
      submitLabel="연결"
      busy={busy}
      onSubmit={submit}
      onClose={props.onClose}
    >
      <label>
        <span>마운트 경로</span>
        <input value={path} onChange={(event) => setPath(event.target.value)} />
      </label>
      <label>
        <span>버킷</span>
        <input autoFocus placeholder="my-bucket" {...field("bucket")} />
      </label>
      <label>
        <span>리전</span>
        <input {...field("region")} />
      </label>
      <label>
        <span>Access key id</span>
        <input {...field("access_key_id")} />
      </label>
      <label>
        <span>Secret access key</span>
        <input type="password" {...field("secret_access_key")} />
      </label>
      <label>
        <span>엔드포인트 (MinIO · R2 — 비워두면 AWS)</span>
        <input placeholder="http://localhost:9000" {...field("endpoint")} />
      </label>
      <label>
        <span>키 접두사 (선택)</span>
        <input placeholder="notes/" {...field("key_prefix")} />
      </label>
    </Modal>
  );
}

export function ResourceDialog(props: {
  kind: ResourceKind;
  onDone: (resource: Resource) => void;
  onClose: () => void;
}) {
  const memory = props.kind === "memory";
  const [name, setName] = useState("");
  const [note, setNote] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      props.onDone(await addResource(props.kind, name, note));
      props.onClose();
    } catch (err) {
      setError(messageOf(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={memory ? "memory 추가" : "docset 추가"}
      hint={
        memory
          ? "워크스페이스 안 /.cortex/memory 에 파일로 만들어집니다. 옆에 놓일 .sqlite 저장소는 cortex-execs/mem 이 붙는 시점에 생깁니다."
          : "워크스페이스 안 /.cortex/docset 에 파일로 만들어집니다. 옆에 놓일 .sqlite 색인은 cortex-execs/index 가 붙는 시점에 생깁니다."
      }
      error={error}
      submitLabel="등록"
      busy={busy}
      onSubmit={submit}
      onClose={props.onClose}
    >
      <label>
        <span>이름</span>
        <input
          autoFocus
          placeholder={memory ? "사용자 선호" : "제품 문서"}
          value={name}
          onChange={(event) => setName(event.target.value)}
        />
      </label>
      <label>
        <span>{memory ? "무엇을 기억할지" : "무엇을 색인할지"} (선택)</span>
        <input
          placeholder={memory ? "대화에서 확정된 사실" : "/docs 아래 마크다운"}
          value={note}
          onChange={(event) => setNote(event.target.value)}
        />
      </label>
    </Modal>
  );
}
