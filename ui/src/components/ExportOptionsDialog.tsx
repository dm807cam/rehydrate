import { useRef, useState } from "react";
import { useDialogA11y } from "../dialogA11y";

interface Props {
  onCancel: () => void;
  onSubmit: (includeAnnotations: boolean, keepDeleted: boolean) => void;
  /** Present when exporting a selection rather than the whole library. */
  selectedItems?: { folder: string; name: string }[];
}

export function ExportOptionsDialog({ onCancel, onSubmit, selectedItems }: Props) {
  const [includeAnnotations, setIncludeAnnotations] = useState(true);
  const [keepDeleted, setKeepDeleted] = useState(true);
  const rootRef = useRef<HTMLDivElement | null>(null);

  const { dialogProps, titleId } = useDialogA11y({
    onEscape: onCancel,
    initialFocusRef: rootRef,
  });

  return (
    <div className="modal-backdrop" onClick={onCancel}>
      <div
        className="modal"
        onClick={(e) => e.stopPropagation()}
        ref={rootRef}
        {...dialogProps}
        tabIndex={-1}
      >
        <h2 id={titleId}>Export Options</h2>
        <p className="muted">
          {selectedItems
            ? `${selectedItems.length} document${selectedItems.length === 1 ? "" : "s"} selected for export.`
            : "Choose what should be included in the exported PDFs."}
        </p>

        {selectedItems && (
          <ul style={{
            margin: "8px 0 16px",
            padding: 0,
            listStyle: "none",
            maxHeight: "220px",
            overflowY: "auto",
            border: "1px solid var(--border)",
            borderRadius: "6px",
          }}>
            {selectedItems.map((item, i) => (
              <li key={i} style={{
                padding: "5px 10px",
                borderBottom: i < selectedItems.length - 1 ? "1px solid var(--border)" : undefined,
                fontSize: "13px",
                lineHeight: "1.4",
              }}>
                {item.folder && (
                  <span style={{ color: "var(--text-muted)", marginRight: "4px" }}>
                    {item.folder} /
                  </span>
                )}
                {item.name}
              </li>
            ))}
          </ul>
        )}

        <label className="checkbox-row" style={{ display: 'flex', alignItems: 'center', gap: '8px', margin: '16px 0 8px' }}>
          <input
            type="checkbox"
            checked={includeAnnotations}
            onChange={(e) => setIncludeAnnotations(e.target.checked)}
          />
          <span>Include pen and highlighter annotations</span>
        </label>

        {!selectedItems && (
          <label className="checkbox-row" style={{ display: 'flex', alignItems: 'center', gap: '8px', margin: '8px 0 16px' }}>
            <input
              type="checkbox"
              checked={keepDeleted}
              onChange={(e) => setKeepDeleted(e.target.checked)}
            />
            <span>Keep local copies of notes deleted from reMarkable</span>
          </label>
        )}

        <div className="actions">
          <button type="button" onClick={onCancel}>
            Cancel
          </button>
          <button type="button" onClick={() => onSubmit(includeAnnotations, keepDeleted)}>
            Choose Folder & Export…
          </button>
        </div>
      </div>
    </div>
  );
}
