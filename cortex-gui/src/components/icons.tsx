// Two monochrome glyphs, inline. A tree of files and folders needs exactly this much iconography,
// and an icon font or an emoji would both bring their own line-height problems for it.

export function FolderIcon({ open }: { open: boolean }) {
  return (
    <svg className="icon" width="13" height="13" viewBox="0 0 16 16" aria-hidden="true">
      <path
        fill="currentColor"
        d={
          open
            ? "M1.5 13.5 3 8h12l-1.6 5.5a1 1 0 0 1-1 .5H2.2a.7.7 0 0 1-.7-.5ZM1 3.2A1.2 1.2 0 0 1 2.2 2H6l1.5 1.6h5.3A1.2 1.2 0 0 1 14 4.8V7H2.6L1 12.2Z"
            : "M2.2 2A1.2 1.2 0 0 0 1 3.2v9.6A1.2 1.2 0 0 0 2.2 14h11.6a1.2 1.2 0 0 0 1.2-1.2V4.8a1.2 1.2 0 0 0-1.2-1.2H7.5L6 2Z"
        }
      />
    </svg>
  );
}

export function FileIcon() {
  return (
    <svg className="icon" width="13" height="13" viewBox="0 0 16 16" aria-hidden="true">
      <path
        fill="currentColor"
        d="M3.5 1h6L13 4.5V14a1 1 0 0 1-1 1H3.5a1 1 0 0 1-1-1V2a1 1 0 0 1 1-1Zm5.7 1.6v2.2h2.2Z"
        opacity="0.85"
      />
    </svg>
  );
}

export function LockIcon() {
  return (
    <svg className="icon" width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
      <path
        fill="currentColor"
        d="M4 7V5a4 4 0 0 1 8 0v2h.5A1.5 1.5 0 0 1 14 8.5v5A1.5 1.5 0 0 1 12.5 15h-9A1.5 1.5 0 0 1 2 13.5v-5A1.5 1.5 0 0 1 3.5 7Zm1.6 0h4.8V5a2.4 2.4 0 1 0-4.8 0Z"
      />
    </svg>
  );
}

export function CheckIcon() {
  return (
    <svg className="icon" width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
      <path fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" d="M3 8.5 6.5 12 13 4.5" />
    </svg>
  );
}

export function CrossIcon() {
  return (
    <svg className="icon" width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
      <path fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" d="M4 4l8 8M12 4l-8 8" />
    </svg>
  );
}

export function ChevronIcon({ open }: { open: boolean }) {
  return (
    <svg className="icon" width="10" height="10" viewBox="0 0 16 16" aria-hidden="true" style={{ transform: open ? "rotate(90deg)" : "none" }}>
      <path fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" d="M6 3l5 5-5 5" />
    </svg>
  );
}

export function ToolIcon() {
  return (
    <svg className="icon" width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
      <path fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" d="M5 3 2 8l3 5M11 3l3 5-3 5" />
    </svg>
  );
}
