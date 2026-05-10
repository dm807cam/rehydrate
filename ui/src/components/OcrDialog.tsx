import { useEffect, useState } from "react";
import { ipc, onOcrProgress } from "../ipc";
import type {
  DocumentSummary,
  OcrProgressEvent,
  OcrStatusReport,
} from "../types";

interface Props {
  document: DocumentSummary;
  onCancel: () => void;
  /** User clicked Convert. The dialog closes immediately;
   * transcription runs in the background and is tracked by the
   * App-level OCR job state (chip + completion toast). */
  onStart: (language: string | undefined) => void;
}

type Phase =
  | { kind: "loading" }
  | { kind: "missing"; descriptor: OcrStatusReport["descriptor"] }
  | { kind: "downloading"; bytes_done: number; bytes_total: number | null }
  | { kind: "model_loading" }
  | { kind: "ready"; descriptor: OcrStatusReport["descriptor"] }
  | { kind: "error"; message: string };

/* "Convert to text…" dialog. Walks the user through (a) downloading
 * the model on first run and (b) loading it into memory; once the
 * model is ready and the user clicks Convert, the dialog hands off
 * to App-level state and closes. The actual transcription progress
 * is shown in the floating `OcrJobChip` so the user can keep
 * working while pages are being processed. */
export function OcrDialog({ document, onCancel, onStart }: Props) {
  const [phase, setPhase] = useState<Phase>({ kind: "loading" });
  const [language, setLanguage] = useState<string>("");
  // Elapsed seconds since the user clicked Download — surfaced in
  // the dialog so a multi-minute load doesn't look like a hang
  // when mistral.rs / hf-hub aren't reporting byte-level progress.
  const [downloadStartedAt, setDownloadStartedAt] = useState<number | null>(
    null,
  );
  const [elapsed, setElapsed] = useState(0);
  useEffect(() => {
    if (downloadStartedAt === null) return;
    const tick = () => setElapsed((Date.now() - downloadStartedAt) / 1000);
    tick();
    const id = window.setInterval(tick, 250);
    return () => window.clearInterval(id);
  }, [downloadStartedAt]);

  // Load model status on mount. Three branches:
  //   - ready: backend already loaded in this process; show
  //     Convert immediately.
  //   - cached: weights are on disk from a prior session; the
  //     backend just needs an in-memory ISQ pass. Auto-trigger
  //     it so the user doesn't have to click a "Load" button
  //     they don't care about.
  //   - missing: weights aren't on disk; ask the user to consent
  //     to a multi-GB download before we start it.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const status = await ipc.ocrStatus();
        if (cancelled) return;
        if (status.kind === "ready") {
          setPhase({ kind: "ready", descriptor: status.descriptor });
        } else if (status.kind === "cached") {
          // Same backend command handles both — it skips the
          // network phase when files are already cached and just
          // runs the load step.
          setDownloadStartedAt(Date.now());
          setElapsed(0);
          setPhase({ kind: "model_loading" });
          try {
            await ipc.ocrDownloadDefaultModel();
            if (cancelled) return;
            const after = await ipc.ocrStatus();
            setDownloadStartedAt(null);
            if (after.kind === "ready") {
              setPhase({ kind: "ready", descriptor: after.descriptor });
            }
          } catch (e) {
            if (!cancelled) {
              setDownloadStartedAt(null);
              setPhase({ kind: "error", message: String(e) });
            }
          }
        } else {
          setPhase({ kind: "missing", descriptor: status.descriptor });
        }
      } catch (e) {
        if (!cancelled) setPhase({ kind: "error", message: String(e) });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Stream model setup events. We deliberately ignore `page_*`
  // events here — those are handled by the App-level OCR job
  // tracker (the floating chip / completion toast), so closing
  // this dialog mid-OCR doesn't lose progress visibility.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onOcrProgress((ev: OcrProgressEvent) => {
      setPhase((current) => {
        switch (ev.kind) {
          case "download_progress":
            return {
              kind: "downloading",
              bytes_done: ev.done,
              bytes_total: ev.total,
            };
          case "model_loading":
            return { kind: "model_loading" };
          case "download_done":
            // Status query at finish settles the descriptor.
            ipc.ocrStatus().then((s) => {
              if (s.kind === "ready") {
                setPhase({ kind: "ready", descriptor: s.descriptor });
              }
            });
            return current;
          default:
            return current;
        }
      });
    }).then((u) => {
      unlisten = u;
    });
    return () => {
      if (unlisten) unlisten();
    };
  }, []);

  async function startDownload() {
    try {
      setDownloadStartedAt(Date.now());
      setElapsed(0);
      setPhase({ kind: "downloading", bytes_done: 0, bytes_total: null });
      await ipc.ocrDownloadDefaultModel();
      const status = await ipc.ocrStatus();
      setDownloadStartedAt(null);
      if (status.kind === "ready") {
        setPhase({ kind: "ready", descriptor: status.descriptor });
      }
    } catch (e) {
      setDownloadStartedAt(null);
      setPhase({ kind: "error", message: String(e) });
    }
  }

  function startTranscribe() {
    // Hand the job to App-level state and close the dialog.
    // Transcription runs in the background; the user sees a
    // floating progress chip and a completion toast.
    onStart(language.trim() || undefined);
  }

  return (
    <div className="modal-backdrop" onClick={onCancel}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>Convert to text</h2>
        <p className="muted">
          On-device handwriting OCR for <strong>{document.visible_name}</strong>.
        </p>

        {phase.kind === "loading" && <p>Checking model…</p>}

        {phase.kind === "missing" && (
          <>
            <p>
              First-time setup: download{" "}
              <strong>{phase.descriptor.display_name}</strong> (
              {(phase.descriptor.size_bytes / 1e9).toFixed(1)} GB). Runs
              entirely on your machine — no notes leave your device.
            </p>
            <div className="actions">
              <button type="button" onClick={onCancel}>
                Cancel
              </button>
              <button className="primary" onClick={startDownload}>
                Download model
              </button>
            </div>
          </>
        )}

        {phase.kind === "downloading" && (
          <>
            <p>Downloading model from HuggingFace…</p>
            <DownloadBar
              done={phase.bytes_done}
              total={phase.bytes_total}
            />
            <p className="muted">
              {phase.bytes_done > 0
                ? `${(phase.bytes_done / 1e9).toFixed(2)} GB${
                    phase.bytes_total
                      ? ` / ${(phase.bytes_total / 1e9).toFixed(2)} GB`
                      : ""
                  } · ${formatElapsed(elapsed)}`
                : `Working — ${formatElapsed(elapsed)} elapsed. Files run a few GB total; this runs entirely on your machine afterwards.`}
            </p>
          </>
        )}

        {phase.kind === "model_loading" && (
          <>
            <p>Loading model into memory…</p>
            <DownloadBar done={0} total={null} />
            <p className="muted">
              The download is on disk. Mapping the weights and quantising to
              4-bit takes a couple of minutes on a consumer machine — your
              fan may spin up briefly. Working — {formatElapsed(elapsed)}{" "}
              elapsed.
            </p>
          </>
        )}

        {phase.kind === "ready" && (
          <>
            <p>
              Using <strong>{phase.descriptor.display_name}</strong>. Optional
              language hint helps with short or ambiguous handwriting.
            </p>
            <input
              type="text"
              placeholder="en, de, ja, ar… (auto-detected if empty)"
              value={language}
              onChange={(e) => setLanguage(e.target.value)}
            />
            <div className="actions">
              <button type="button" onClick={onCancel}>
                Cancel
              </button>
              <button className="primary" onClick={startTranscribe}>
                Convert
              </button>
            </div>
          </>
        )}

        {phase.kind === "error" && (
          <>
            <div className="error inline">{phase.message}</div>
            <div className="actions">
              <button type="button" onClick={onCancel}>
                Close
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/* Custom progress strip. WebKit's bare `<progress>` element renders
 * as a thin near-invisible sliver, especially in indeterminate
 * mode — replace it with a clearly-sized div whose fill is either
 * a determinate width (when bytes_total is known) or an animated
 * shimmer (mistral.rs / hf-hub don't surface byte-level events). */
function DownloadBar({
  done,
  total,
}: {
  done: number;
  total: number | null;
}) {
  const determinate = total != null && total > 0 && done > 0;
  const pct = determinate ? Math.min(100, (done / (total as number)) * 100) : 30;
  return (
    <div className="ocr-download-bar" role="progressbar">
      <span
        className={determinate ? "" : "indeterminate"}
        style={{ width: `${pct}%` }}
      />
    </div>
  );
}

function formatElapsed(seconds: number): string {
  const total = Math.floor(seconds);
  const m = Math.floor(total / 60);
  const s = total % 60;
  if (m === 0) return `${s} s`;
  return `${m}:${s.toString().padStart(2, "0")}`;
}
