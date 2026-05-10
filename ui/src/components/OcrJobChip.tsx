/* Floating progress chip shown while a background OCR job is in
 * flight. Lives at the App level so closing the OcrDialog (or
 * navigating away from the document) doesn't lose progress
 * visibility. The actual job is a single in-flight call to
 * `ipc.transcribeDocument`; per-page deltas come over `ocr:progress`
 * events handled in App.tsx. We render a small pill that animates
 * its width across the page count when known, and a label that
 * stays readable at the chip's compact size.
 *
 * Click cancels the job (best-effort — the IPC call doesn't yet
 * support cancellation, so this just hides the chip; the job keeps
 * running on the backend until done. Wire to a real cancel signal
 * later if cancellation matters). */
export interface OcrJob {
  documentId: string;
  visibleName: string;
  phase: "running" | "done" | "error";
  /** Pages completed so far, per `page_done` events. */
  pagesDone: number;
  /** Cumulative chars across pages — updates as work proceeds. */
  charCount: number;
  /** ms timestamp; used for elapsed-time display. */
  startedAt: number;
  /** Final page count once `transcribe_document` returns. */
  totalPages?: number;
  /** Set when phase = "error". */
  error?: string;
}

export function OcrJobChip({
  job,
  elapsedSeconds,
  onDismiss,
}: {
  job: OcrJob;
  elapsedSeconds: number;
  onDismiss: () => void;
}) {
  if (job.phase !== "running") return null;
  const pageLabel = job.pagesDone === 1 ? "page" : "pages";
  // We don't know the total page count until the IPC call returns,
  // so render an indeterminate-looking bar that grows as more pages
  // come in. Width is capped at 80% so the chip never looks "done"
  // before it actually is.
  const pct = Math.min(80, 8 + job.pagesDone * 6);
  return (
    <div className="ocr-chip" role="status" aria-live="polite">
      <div className="ocr-chip-bar">
        <span className="indeterminate" style={{ width: `${pct}%` }} />
      </div>
      <div className="ocr-chip-text">
        <strong>OCR · {job.visibleName}</strong>
        <span className="muted small">
          {job.pagesDone} {pageLabel} · {job.charCount.toLocaleString()} chars · {formatElapsed(elapsedSeconds)}
        </span>
      </div>
      <button
        type="button"
        className="ocr-chip-close"
        aria-label="Hide progress"
        onClick={onDismiss}
        title="Hide — OCR keeps running in the background"
      >
        ×
      </button>
    </div>
  );
}

function formatElapsed(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  const m = Math.floor(total / 60);
  const s = total % 60;
  if (m === 0) return `${s}s`;
  return `${m}:${s.toString().padStart(2, "0")}`;
}
