// OCR + auto-OCR-at-startup sweep — extracted from App.tsx.
//
// The hook owns:
//   - `ocrJob` / `ocrJobElapsed` — the floating progress chip's data.
//   - `autoOcrSweep` — when the user has opted in via Settings, the
//     queue/done/totalAtStart triple that drives the chip's "(N of M)"
//     batch progress.
//   - The single `ocr:progress` event listener that updates per-page
//     counters as work proceeds.
//   - The elapsed-time tick that runs only while a job is in flight.
//
// And exposes the three imperative actions:
//   - `startOcrJob(doc, lang?)` — single-doc transcription, the
//     "Convert to text…" menu action.
//   - `startAutoOcrSweep()` — bulk transcription, fired once after
//     the library opens when the auto-OCR opt-in is on.
//   - `dismissOcrChip()` — what the chip's × button calls. In sweep
//     mode it bumps the cancellation token (stops the whole queue);
//     in single-job mode it just hides the chip (the IPC call has no
//     cancellation handle, see the comment in OcrJobChip.tsx).
//
// Token-based cancellation: every sweep captures an incrementing
// integer. The loop checks the token between docs; a new sweep bumps
// the token and silently invalidates any in-flight sweep. A
// `useRef` boolean would have left a window where a new sweep could
// flip the flag back to true after the previous sweep had already
// short-circuited.

import type { ReactNode } from "react";
import { useCallback, useEffect, useRef, useState } from "react";

import { formatError } from "../formatError";
import { ipc, onOcrProgress } from "../ipc";
import { parseOllamaUnconfigured } from "../components/settingsErrors";
import type { DocumentSummary, OcrJob, OcrSweepProgress } from "../types";

export interface UseOcrInjections {
  /// Refresh the library lists after a successful OCR completion (so
  /// the new transcript shows up in the document menu / drawer).
  refreshLibrary: () => Promise<void>;
  /// Show a toast. The hook surfaces success / per-doc failure / sweep
  /// summary; the App owns the toaster.
  toast: {
    show: (input: {
      tone: "info" | "ok" | "warn" | "err";
      duration?: number;
      body: ReactNode;
      action?: { label: string; onClick: () => void | Promise<void> };
    }) => number;
  };
  /// When OCR fails because Ollama isn't configured, route the user
  /// into Settings → Ollama instead of toasting a raw error.
  openSettings: (tab: "ollama" | "publishing", banner?: string) => void;
  /// "View transcript" action on the success toast.
  openTranscript: (doc: DocumentSummary) => void;
}

export interface UseOcrResult {
  ocrJob: OcrJob | null;
  ocrJobElapsed: number;
  autoOcrSweep: OcrSweepProgress | null;
  startOcrJob: (doc: DocumentSummary, language?: string) => void;
  startAutoOcrSweep: () => Promise<void>;
  dismissOcrChip: () => void;
}

export function useOcr(injections: UseOcrInjections): UseOcrResult {
  // Stash injections in a ref. App-side callers pass inline arrows
  // for `openSettings` / `openTranscript` / the `refreshLibrary`
  // re-export that change identity each render; without the ref,
  // every `useCallback` below would be re-created per render and
  // defeat the entire point of memoization. See `useLibrary` for
  // the same pattern.
  const injectionsRef = useRef(injections);
  injectionsRef.current = injections;

  const [ocrJob, setOcrJob] = useState<OcrJob | null>(null);
  const [ocrJobElapsed, setOcrJobElapsed] = useState(0);
  const [autoOcrSweep, setAutoOcrSweep] = useState<
    | (OcrSweepProgress & {
        queue: { documentId: string; visibleName: string }[];
      })
    | null
  >(null);

  // App-level listener for per-page progress events from the Rust
  // side. One subscription for the hook's lifetime so the chip keeps
  // updating regardless of which dialog/drawer the user has open.
  useEffect(() => {
    // Issue #37: close the unmount-before-resolve race. Without the
    // `cancelled` flag, an OCR job firing after this hook's host
    // unmounts (library swap, user navigation) would keep calling
    // setOcrJob on dead state forever.
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    onOcrProgress((ev) => {
      setOcrJob((cur) => {
        if (!cur || cur.phase !== "running") return cur;
        if (ev.kind === "page_done") {
          // Count distinct page_done events rather than tracking
          // `max(page_index) + 1`. The backend reports notebook
          // indices (remapped from its internal slice-index in
          // `transcribe_document`), and blank pages are emitted up
          // front as synthetic page_done events with chars: 0. With
          // the old max+1 formula a synth event for notebook index
          // 4 would jump-set pagesDone to 5 even though only one
          // page was actually processed — counting events keeps the
          // chip's "X pages" tally honest regardless of arrival
          // order.
          return {
            ...cur,
            pagesDone: cur.pagesDone + 1,
            charCount: cur.charCount + ev.chars,
          };
        }
        if (ev.kind === "page_failed") {
          // Don't abort the chip — the run continues with the rest
          // of the pages. The chip stays in "running" until the IPC
          // promise settles.
          return cur;
        }
        return cur;
      });
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  // Elapsed timer ticks at 500 ms while a job is running; pauses
  // otherwise to avoid the unmount → remount → re-fire pattern.
  useEffect(() => {
    if (!ocrJob || ocrJob.phase !== "running") return;
    const tick = () =>
      setOcrJobElapsed((Date.now() - ocrJob.startedAt) / 1000);
    tick();
    const id = window.setInterval(tick, 500);
    return () => window.clearInterval(id);
  }, [ocrJob]);

  const startOcrJob = useCallback(
    (doc: DocumentSummary, language?: string) => {
      const { toast } = injectionsRef.current;
      setOcrJob((cur) => {
        if (cur && cur.phase === "running") {
          toast.show({
            tone: "warn",
            body: `Already transcribing "${cur.visibleName}" — wait for that to finish first.`,
          });
          return cur;
        }
        return {
          documentId: doc.document_id,
          visibleName: doc.visible_name,
          phase: "running",
          pagesDone: 0,
          charCount: 0,
          startedAt: Date.now(),
        };
      });

      void (async () => {
        try {
          const summary = await ipc.transcribeDocument(
            doc.document_id,
            language ?? null,
          );
          setOcrJob((cur) =>
            cur && cur.documentId === doc.document_id
              ? {
                  ...cur,
                  phase: "done",
                  pagesDone: summary.page_count,
                  charCount: summary.char_count,
                  totalPages: summary.page_count,
                }
              : cur,
          );
          const { refreshLibrary, toast, openTranscript } =
            injectionsRef.current;
          await refreshLibrary();
          // Render the doc name in `<strong>` so the visual anchor
          // of the toast survives — `styles.css .toast .toast-body
          // strong` restores full-strength text against the muted
          // body. The earlier hook extraction had collapsed this
          // to a plain template string and dropped the styling.
          toast.show({
            tone: "ok",
            duration: 8000,
            body: (
              <>
                Transcribed <strong>{doc.visible_name}</strong> —{" "}
                {summary.page_count}{" "}
                {summary.page_count === 1 ? "page" : "pages"},{" "}
                {summary.char_count.toLocaleString()} characters.
              </>
            ),
            action: {
              label: "View",
              onClick: () => openTranscript(doc),
            },
          });
          setOcrJob(null);
        } catch (e) {
          const unconfigured = parseOllamaUnconfigured(e);
          if (unconfigured) {
            setOcrJob(null);
            injectionsRef.current.openSettings("ollama", unconfigured.message);
            return;
          }
          setOcrJob((cur) =>
            cur && cur.documentId === doc.document_id
              ? { ...cur, phase: "error", error: formatError(e) }
              : cur,
          );
          injectionsRef.current.toast.show({
            tone: "err",
            duration: 0,
            body: (
              <>
                OCR failed for <strong>{doc.visible_name}</strong>:{" "}
                {formatError(e)}
              </>
            ),
          });
          setOcrJob(null);
        }
      })();
    },
    [],
  );

  const sweepTokenRef = useRef(0);

  const startAutoOcrSweep = useCallback(async () => {
    const myToken = ++sweepTokenRef.current;
    const stillOurs = () => sweepTokenRef.current === myToken;

    const cfg = await ipc.getOllamaConfig();
    if (!cfg.auto_ocr_on_startup) return;
    // Pre-flight ping. Silent on failure — daemon not running at
    // launch is a normal-life state, not a thing to nag about.
    const probe = await ipc.pingOllama(cfg.base_url);
    if (!stillOurs()) return;
    if (!probe.ok) {
      console.info(
        `Auto-OCR sweep skipped: Ollama not reachable at ${cfg.base_url}`,
      );
      return;
    }
    const raw = await ipc.listDocumentsNeedingOcr();
    if (!stillOurs()) return;
    if (raw.length === 0) return;
    const candidates = raw.map((c) => ({
      documentId: c.document_id,
      visibleName: c.visible_name,
    }));
    setAutoOcrSweep({
      queue: candidates,
      totalAtStart: candidates.length,
      done: 0,
    });
    let transcribed = 0;
    let skipped = 0;
    let abortedMidSweep = false;
    for (const c of candidates) {
      if (!stillOurs()) return;
      setOcrJob({
        documentId: c.documentId,
        visibleName: c.visibleName,
        phase: "running",
        pagesDone: 0,
        charCount: 0,
        startedAt: Date.now(),
      });
      try {
        await ipc.transcribeDocument(c.documentId, null);
        transcribed += 1;
      } catch (e) {
        if (!stillOurs()) return;
        const unconfigured = parseOllamaUnconfigured(e);
        if (unconfigured) {
          // Ollama went away mid-sweep — stop quietly.
          console.info(
            `Auto-OCR sweep aborted mid-pass: ${unconfigured.message}`,
          );
          abortedMidSweep = true;
          break;
        }
        // Per-doc failure (render error, etc.) — skip and continue.
        // The id is enough to locate the doc in the library; the
        // visible name is deliberately not logged.
        console.warn(`Auto-OCR skipped doc ${c.documentId}: ${e}`);
        skipped += 1;
      }
      if (!stillOurs()) return;
      setAutoOcrSweep((prev) =>
        prev
          ? {
              ...prev,
              done: prev.done + 1,
              queue: prev.queue.slice(1),
            }
          : null,
      );
    }
    if (!stillOurs()) return;
    setOcrJob(null);
    setAutoOcrSweep(null);
    const { refreshLibrary, toast } = injectionsRef.current;
    if (transcribed > 0 || skipped > 0) {
      await refreshLibrary();
      toast.show({
        tone: "ok",
        duration: 8000,
        body:
          skipped === 0
            ? `Auto-OCR finished — transcribed ${transcribed} notebook${transcribed === 1 ? "" : "s"}.`
            : `Auto-OCR finished — transcribed ${transcribed}, skipped ${skipped}.`,
      });
    } else if (abortedMidSweep) {
      toast.show({
        tone: "warn",
        duration: 9000,
        body: "Auto-OCR couldn't reach Ollama. Open Settings → Ollama to test the connection.",
      });
    }
  }, []);

  // Chip × button:
  //   * Invalidates the sweep token (universal "stop any in-flight
  //     sweep" lever) so the loop between docs unwinds.
  //   * Trips the shared OCR cancel flag so the *current* document's
  //     transcribe call aborts at the next page boundary. Without
  //     this the user could be staring at a 200-page notebook for
  //     20+ minutes with no escape.
  //   * Clears local UI state so the chip vanishes promptly. The
  //     underlying IPC call may still take a beat to return as the
  //     engine finishes the in-flight page.
  const dismissOcrChip = useCallback(() => {
    sweepTokenRef.current += 1;
    ipc.cancelOcr().catch(() => {
      /* idempotent; the next OCR job resets the flag anyway */
    });
    setOcrJob(null);
    setAutoOcrSweep(null);
  }, []);

  return {
    ocrJob,
    ocrJobElapsed,
    autoOcrSweep,
    startOcrJob,
    startAutoOcrSweep,
    dismissOcrChip,
  };
}
