import type { FormEvent, ReactNode } from "react";

interface Props {
  title: string;
  hint?: string;
  error?: string | null;
  submitLabel: string;
  busy?: boolean;
  onSubmit: () => void;
  onClose: () => void;
  children: ReactNode;
}

/** The one dialog shape every form in this window uses. */
export default function Modal(props: Props) {
  const submit = (event: FormEvent) => {
    event.preventDefault();
    props.onSubmit();
  };
  return (
    <div
      className="scrim"
      // `mousedown` on the scrim itself, so a drag that starts inside the dialog and ends on the
      // backdrop — selecting the last word of a long field — does not dismiss what was typed.
      onMouseDown={(event) => event.target === event.currentTarget && props.onClose()}
    >
      <form className="modal" onSubmit={submit}>
        <h3>{props.title}</h3>
        {props.hint && <p className="hint">{props.hint}</p>}
        {props.error && <p className="error">{props.error}</p>}
        {props.children}
        <div className="buttons">
          <button type="button" onClick={props.onClose}>
            취소
          </button>
          <button type="submit" className="primary" disabled={props.busy}>
            {props.busy ? "…" : props.submitLabel}
          </button>
        </div>
      </form>
    </div>
  );
}
