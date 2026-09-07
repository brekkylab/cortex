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
