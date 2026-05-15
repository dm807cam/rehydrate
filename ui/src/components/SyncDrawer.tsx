import { useEffect, useMemo, useRef, useState } from "react";
import { ipc, onSyncPhase, onSyncProgress } from "../ipc";
import { Icon } from "./Icon";
import { Skeleton } from "./Skeleton";
import { humanizeSyncError } from "../humanizeError";
import { useDialogA11y } from "../dialogA11y";
import type {
  PlanItemStatus,
  ProgressEvent,
  PullPlan,
  PushItemStatus,
  PushPlan,
  TwoWayReport,
} from "../types";

interface Props {
  onClose: () => void;
  onComplete: () => void;
  /** Notified whenever the drawer's sync state changes:
   *  - "idle":    drawer is showing the plan or done state, not running.
   *  - "syncing": a sync is currently in progress.
   *  - "failed":  the most recent sync ended with an error.
   *
   *  The App uses this to drive the toolbar StatusPill (which used
   *  to be wired to a boolean `running` flag and therefore couldn't
   *  surface failures — they appeared as a clean "idle" instead). */
  onSyncStateChange?: (phase: "idle" | "syncing" | "failed") => void;
}

interface DocProgress {
  state: "queued" | "in-progress" | "done" | "skipped";
  files: number;
  bytes: number;
  totalFiles?: number;
  reason?: string;
}

const PULL_LABEL: Record<PlanItemStatus, string> = {
  new: "New",
  changed: "Updated",
  unchanged: "Unchanged",
  skipped: "Skipped",
};
const PUSH_LABEL: Record<PushItemStatus, string> = {
  outbound: "Upload",
  unchanged: "Unchanged",
  skipped: "Skipped",
};

export function SyncDrawer({ onClose, onComplete, onSyncStateChange }: Props) {
  const { dialogProps, rootRef, titleId } = useDialogA11y({
    onEscape: onClose,
  });
  const [pullPlan, setPullPlan] = useState<PullPlan | null>(null);
  const [pushPlan, setPushPlan] = useState<PushPlan | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  // True between "user clicked cancel" and "engine actually returns".
  // Bounded by the engine's per-document polling cadence (a few
  // hundred ms typical, up to one full document on a slow link).
  const [cancelling, setCancelling] = useState(false);
  const [phase, setPhase] = useState<"plan" | "pull" | "push" | "done">("plan");
  const [report, setReport] = useState<TwoWayReport | null>(null);
  const [progress, setProgress] = useState<Record<string, DocProgress>>({});
  // Non-fatal warnings the engine emits (e.g. push uploaded everything
  // but the tablet's xochitl restart failed). Surfaced as a card below
  // the success state — the sync still counts as completed.
  const [warnings, setWarnings] = useState<string[]>([]);
  const startedAtRef = useRef<number | null>(null);
  // Issue #37: SyncDrawer attaches sync-phase / sync-progress
  // listeners inside the async `start()` flow, and only unlistens
  // them in the inner `finally`. If the drawer unmounts while
  // `start()` is mid-`await ipc.syncTwoWay()`, the listeners run
  // for the duration of the in-flight IPC call against a dead
  // component, calling setProgress / setPhase on unmounted state
  // and (in dev StrictMode) doubling the leak. Track every
  // in-flight unlisten so the component-unmount cleanup below can
  // detach them too.
  const inFlightUnlisteners = useRef<Array<() => void>>([]);
  useEffect(() => {
    return () => {
      for (const u of inFlightUnlisteners.current) {
        u();
      }
      inFlightUnlisteners.current = [];
    };
  }, []);

  // Mirror the drawer's sync state up to the parent. The "failed"
  // branch matters: a sync error used to look indistinguishable from
  // a successful completion at the toolbar level because the
  // boolean-running mirror always collapsed to "idle" in the finally
  // block. Tracking planError as part of the projection means the
  // pill keeps showing red until the user starts a new sync.
  const syncState: "idle" | "syncing" | "failed" = running
    ? "syncing"
    : planError
      ? "failed"
      : "idle";
  useEffect(() => {
    onSyncStateChange?.(syncState);
  }, [syncState, onSyncStateChange]);

  // Build the plan preview up front so the user can see what's about
  // to happen before they hit Start.
  useEffect(() => {
    let mounted = true;
    Promise.all([ipc.pullPlan(), ipc.pushPlan()])
      .then(([pl, ps]) => {
        if (!mounted) return;
        setPullPlan(pl);
        setPushPlan(ps);
      })
      .catch((e) => {
        if (mounted) setPlanError(humanizeSyncError(e));
      });
    return () => {
      mounted = false;
    };
  }, []);

  const counts = useMemo(() => {
    if (!pullPlan || !pushPlan) return null;
    const pull = { new: 0, changed: 0, unchanged: 0, skipped: 0 };
    for (const item of pullPlan.items) pull[item.status]++;
    const push = { outbound: 0, unchanged: 0, skipped: 0 };
    for (const item of pushPlan.items) push[item.status]++;
    return {
      incoming: pull.new + pull.changed,
      outgoing: push.outbound,
      ...pull,
      ...push,
    };
  }, [pullPlan, pushPlan]);

  // Items grouped by direction. Skip "unchanged" — they're noise.
  const groups = useMemo(() => {
    const incoming: Array<RowItem> = [];
    const outgoing: Array<RowItem> = [];
    if (pullPlan) {
      for (const i of pullPlan.items) {
        if (i.status === "unchanged") continue;
        incoming.push({
          uuid: i.entry.uuid,
          title: i.entry.visible_name || i.entry.uuid,
          docType: i.entry.doc_type,
          label: PULL_LABEL[i.status],
          variant: i.status === "skipped" ? "skipped" : "primary",
          direction: "pull",
        });
      }
    }
    if (pushPlan) {
      for (const i of pushPlan.items) {
        if (i.status === "unchanged") continue;
        outgoing.push({
          uuid: i.document.document_id,
          title: i.document.visible_name,
          docType: i.document.doc_type,
          label: PUSH_LABEL[i.status],
          variant: i.status === "skipped" ? "skipped" : "primary",
          direction: "push",
        });
      }
    }
    return { incoming, outgoing };
  }, [pullPlan, pushPlan]);

  const totalActive = groups.incoming.length + groups.outgoing.length;
  const completedCount = useMemo(
    () => Object.values(progress).filter((p) => p.state === "done" || p.state === "skipped").length,
    [progress],
  );

  async function start() {
    setRunning(true);
    setCancelling(false);
    setProgress({});
    setReport(null);
    setWarnings([]);
    setPlanError(null);
    setPhase("pull");
    startedAtRef.current = Date.now();

    const unlistenPhase = await onSyncPhase((p) => setPhase(p));
    inFlightUnlisteners.current.push(unlistenPhase);
    const unlistenProgress = await onSyncProgress((ev: ProgressEvent) => {
      setProgress((prev) => {
        const next = { ...prev };
        switch (ev.kind) {
          case "document_started":
            next[ev.document_id] = {
              state: "in-progress",
              files: 0,
              bytes: 0,
            };
            break;
          case "file_fetched": {
            const cur = next[ev.document_id] ?? {
              state: "in-progress" as const,
              files: 0,
              bytes: 0,
            };
            next[ev.document_id] = {
              ...cur,
              state: "in-progress",
              files: cur.files + 1,
              bytes: cur.bytes + ev.bytes,
            };
            break;
          }
          case "document_completed": {
            const cur = next[ev.document_id] ?? {
              state: "done" as const,
              files: 0,
              bytes: 0,
            };
            next[ev.document_id] = { ...cur, state: "done" };
            break;
          }
          case "document_skipped":
            next[ev.document_id] = {
              state: "skipped",
              files: 0,
              bytes: 0,
              reason: ev.reason,
            };
            break;
          case "warning":
            // Warnings are session-level (not per-doc), so collect
            // them outside the doc-keyed progress map. Effect below
            // pulls them into `warnings` after `setProgress` returns.
            queueMicrotask(() =>
              setWarnings((prev) => [...prev, ev.message]),
            );
            break;
        }
        return next;
      });
    });
    inFlightUnlisteners.current.push(unlistenProgress);

    try {
      const r = await ipc.syncTwoWay();
      setReport(r);
      setPhase("done");
      onComplete();
      // Auto-dismiss when there were no skips/errors. Gives the user
      // confirmation flash without requiring a click.
      const cleanFinish =
        r.pull.skipped === 0 && r.push.skipped === 0;
      if (cleanFinish) {
        setTimeout(() => onClose(), 1500);
      }
    } catch (e) {
      setPlanError(humanizeSyncError(e));
    } finally {
      unlistenPhase();
      unlistenProgress();
      // Remove from the in-flight set so the component-unmount
      // cleanup doesn't call them a second time (russh-sftp's
      // unlisten is idempotent today, but we shouldn't rely on it).
      inFlightUnlisteners.current = inFlightUnlisteners.current.filter(
        (u) => u !== unlistenPhase && u !== unlistenProgress,
      );
      setRunning(false);
      setCancelling(false);
    }
  }

  return (
    <div
      className="drawer"
      onClick={(e) => e.stopPropagation()}
      ref={rootRef}
      {...dialogProps}
    >
      <header>
        <h2 id={titleId}>Sync with reMarkable</h2>
        <button
          onClick={onClose}
          className="close"
          aria-label="Close"
          title={
            running
              ? "Hide — sync keeps running; the toolbar shows progress"
              : "Close"
          }
        >
          ×
        </button>
      </header>

      {planError && (
        <div className="error">
          <Icon name="warn" />
          <span style={{ flex: 1 }}>{planError}</span>
          <button onClick={start} disabled={running}>
            <Icon name="sync" /> Retry
          </button>
        </div>
      )}
      {warnings.length > 0 && (
        <div className="warning-card">
          <Icon name="warn" />
          <div className="body">
            <strong>
              Sync completed with {warnings.length === 1
                ? "a warning"
                : `${warnings.length} warnings`}
            </strong>
            <ul>
              {warnings.map((w, i) => (
                <li key={i}>{w}</li>
              ))}
            </ul>
          </div>
        </div>
      )}

      {!pullPlan || !pushPlan ? (
        <div className="empty">
          <Skeleton width={200} height={26} mb={12} />
          <Skeleton width={280} mb={8} />
          <Skeleton width={240} />
        </div>
      ) : (
        <>
          {!report && (
            <div className="sync-hero">
              <div className="nums">
                <span
                  className={`num ${counts && counts.incoming === 0 ? "idle" : ""}`}
                  title="Incoming from the tablet"
                >
                  <Icon name="arrowDown" />
                  {counts?.incoming ?? 0}
                  <small>incoming</small>
                </span>
                <span
                  className={`num ${counts && counts.outgoing === 0 ? "idle" : ""}`}
                  title="Outgoing to the tablet"
                >
                  <Icon name="arrowUp" />
                  {counts?.outgoing ?? 0}
                  <small>to upload</small>
                </span>
              </div>
              {running ? (
                <button
                  className="primary start"
                  onClick={() => {
                    // Cancellation is cooperative: the engine polls
                    // the flag between documents, so the button
                    // dims to "Cancelling…" until the in-flight
                    // chunk completes and the cancel actually lands.
                    ipc.cancelSync().catch(() => {
                      /* idempotent — if the call fails the sync
                         either already finished or never started */
                    });
                    setCancelling(true);
                  }}
                  disabled={cancelling}
                >
                  <Icon name="sync" />
                  {cancelling ? "Cancelling…" : "Cancel sync"}
                </button>
              ) : (
                <button
                  className="primary start"
                  onClick={start}
                  disabled={totalActive === 0}
                >
                  {totalActive === 0 ? (
                    "Nothing to sync"
                  ) : (
                    <>
                      <Icon name="sync" /> Start sync
                    </>
                  )}
                </button>
              )}
            </div>
          )}

          {report && (
            <div className="success-card">
              <Icon name="check" />
              <div className="body">
                <strong>{successHeadline(report)}</strong>
                <span>{successDetail(report, startedAtRef.current)}</span>
              </div>
              <button onClick={onClose}>Close</button>
            </div>
          )}

          {!report && totalActive > 0 && (
            <div className="plan-list" style={{ overflowY: "auto" }}>
              {groups.incoming.length > 0 && (
                <>
                  <div className="sync-section">
                    <Icon name="arrowDown" /> From the tablet · {groups.incoming.length}
                  </div>
                  {groups.incoming.map((it) => (
                    <SyncRow
                      key={`pull-${it.uuid}`}
                      item={it}
                      progress={progress[it.uuid]}
                      activePhase={phase}
                    />
                  ))}
                </>
              )}
              {groups.outgoing.length > 0 && (
                <>
                  <div className="sync-section">
                    <Icon name="arrowUp" /> To the tablet · {groups.outgoing.length}
                  </div>
                  {groups.outgoing.map((it) => (
                    <SyncRow
                      key={`push-${it.uuid}`}
                      item={it}
                      progress={progress[it.uuid]}
                      activePhase={phase}
                    />
                  ))}
                </>
              )}
            </div>
          )}

          {!report && totalActive === 0 && (
            <div className="empty">
              <h2>Already in sync</h2>
              <p>Nothing has changed on the tablet or in the library since the last sync.</p>
            </div>
          )}

          {/* Global progress bar — shows determinate fill while running. */}
          {running && (
            <div className="global-progress">
              <span
                className={completedCount === 0 ? "indeterminate" : ""}
                style={{
                  width: totalActive > 0
                    ? `${Math.round((completedCount / totalActive) * 100)}%`
                    : "0%",
                }}
              />
            </div>
          )}

          {!report && (
            <footer>
              {running && (
                <span className="muted small">
                  {phase === "pull"
                    ? "Pulling from the tablet…"
                    : phase === "push"
                      ? "Uploading to the tablet…"
                      : "Working…"}{" "}
                  · {completedCount}/{totalActive}
                </span>
              )}
              {!running && totalActive > 0 && (
                <span className="muted small">
                  {totalActive} document{totalActive === 1 ? "" : "s"} ready to move
                </span>
              )}
            </footer>
          )}
        </>
      )}
    </div>
  );
}

interface RowItem {
  uuid: string;
  title: string;
  docType: string;
  label: string;
  variant: "primary" | "skipped";
  direction: "pull" | "push";
}

function SyncRow({
  item,
  progress,
  activePhase,
}: {
  item: RowItem;
  progress: DocProgress | undefined;
  activePhase: "plan" | "pull" | "push" | "done";
}) {
  const inThisPhase = activePhase === item.direction;
  const isInProgress = inThisPhase && progress?.state === "in-progress";
  const isDone = progress?.state === "done";
  const isSkipped = progress?.state === "skipped";
  // Pseudo-progress: each file_fetched bumps the bar a bit. Without a
  // pre-known totalFiles we use a smooth ease toward 90% and snap to
  // 100% on completion.
  const pct = isDone
    ? 100
    : isSkipped
      ? 100
      : isInProgress
        ? Math.min(90, (progress?.files ?? 0) * 8 + 12)
        : 0;
  const cls = `sync-row${isInProgress ? " in-progress" : ""}${isDone || isSkipped ? " done" : ""}`;
  return (
    <div className={cls}>
      <span
        className={`badge ${item.variant === "skipped" ? "badge-skipped" : "badge-new"}`}
      >
        {item.label}
      </span>
      <span className="title">{item.title}</span>
      <span className="muted small">{prettyDocType(item.docType)}</span>
      {isInProgress && progress && (
        <span className="muted small">
          {progress.files} file{progress.files === 1 ? "" : "s"} · {formatBytes(progress.bytes)}
        </span>
      )}
      {isDone && <Icon name="check" className="icon-done" />}
      {isSkipped && (
        <span className="badge badge-skipped" title={progress?.reason}>
          <Icon name="warn" /> Skipped
        </span>
      )}
      {(isInProgress || isDone) && (
        <span className="progress-bar" style={{ width: `${pct}%` }} />
      )}
    </div>
  );
}

function prettyDocType(s: string): string {
  if (s === "Notebook") return "Notebook";
  if (s === "DocumentType.Pdf") return "PDF";
  if (s === "DocumentType.Epub") return "EPUB";
  return s;
}

function successHeadline(r: TwoWayReport): string {
  const moved = r.pull.recorded + r.push.pushed;
  if (moved === 0) return "Already in sync";
  if (r.pull.recorded > 0 && r.push.pushed === 0)
    return `Pulled ${r.pull.recorded} document${r.pull.recorded === 1 ? "" : "s"} from your tablet`;
  if (r.push.pushed > 0 && r.pull.recorded === 0)
    return `Sent ${r.push.pushed} document${r.push.pushed === 1 ? "" : "s"} to your tablet`;
  return `Synced ${moved} document${moved === 1 ? "" : "s"}`;
}

function successDetail(r: TwoWayReport, startedAt: number | null): string {
  const took = startedAt ? Math.max(1, Math.round((Date.now() - startedAt) / 1000)) : null;
  const skipped = r.pull.skipped + r.push.skipped;
  const parts: string[] = [];
  if (took !== null) parts.push(`in ${took}s`);
  if (skipped > 0) parts.push(`${skipped} skipped`);
  return parts.length > 0 ? parts.join(" · ") : "Library and tablet are aligned.";
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}
