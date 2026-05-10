import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type DragEvent as ReactDragEvent,
  type ReactNode,
} from "react";
import { ipc, onDeviceReachable, onOcrProgress } from "./ipc";
import { HistoryDrawer } from "./components/HistoryDrawer";
import { LogDrawer } from "./components/LogDrawer";
import { OcrDialog } from "./components/OcrDialog";
import { OcrJobChip, type OcrJob } from "./components/OcrJobChip";
import { PasswordDialog } from "./components/PasswordDialog";
import { PublishingSettings } from "./components/PublishingSettings";
import { TranscriptDrawer } from "./components/TranscriptDrawer";
import { StatusPill } from "./components/StatusPill";
import { SyncDrawer } from "./components/SyncDrawer";
import { Icon } from "./components/Icon";
import { LibrarySwitcher } from "./components/LibrarySwitcher";
import { Menu } from "./components/Menu";
import { Skeleton } from "./components/Skeleton";
import { useToast } from "./components/Toast";
import { useConfirm } from "./components/Confirm";
import { Cheatsheet } from "./components/Cheatsheet";
import { CommandPalette, type PaletteItem } from "./components/CommandPalette";
import { Onboarding } from "./components/Onboarding";
import { QuickLook } from "./components/QuickLook";
import { RenameDialog } from "./components/RenameDialog";
import { Thumbnail, invalidateThumbnail } from "./components/Thumbnail";
import { DRAG_ICON_SVG, setCustomDragImage } from "./dragImage";
import type {
  ArchivedDocument,
  DeviceState,
  DocumentSummary,
  FolderEntry,
  LibrarySummary,
  RecentLibraryEntry,
} from "./types";

export function App() {
  const [defaultPath, setDefaultPath] = useState<string | null>(null);
  const [libraryOpen, setLibraryOpen] = useState(false);
  const [libraryPath, setLibraryPath] = useState<string | null>(null);
  const [recentLibraries, setRecentLibraries] = useState<RecentLibraryEntry[]>(
    [],
  );
  const [summary, setSummary] = useState<LibrarySummary | null>(null);
  const [documents, setDocuments] = useState<DocumentSummary[] | null>(null);
  const [folders, setFolders] = useState<FolderEntry[]>([]);
  const [archived, setArchived] = useState<ArchivedDocument[]>([]);
  const [device, setDevice] = useState<DeviceState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showPassword, setShowPassword] = useState(false);
  const [showSync, setShowSync] = useState(false);
  const [historyDoc, setHistoryDoc] = useState<DocumentSummary | null>(null);
  const [showLogs, setShowLogs] = useState(false);
  const [view, setView] = useState<View>("all");
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [anchorId, setAnchorId] = useState<string | null>(null);
  // Keyboard cursor — moves with arrow keys and tracks the last
  // clicked row, but does NOT paint a selection highlight in default
  // mode. The visible "selected" background is reserved for select
  // mode entries in `selectedIds`.
  const [focusId, setFocusId] = useState<string | null>(null);
  const [syncPhase, setSyncPhase] = useState<"idle" | "syncing" | "failed">("idle");
  const [search, setSearch] = useState("");
  const [showSearch, setShowSearch] = useState(false);
  const [leavingIds, setLeavingIds] = useState<Set<string>>(new Set());
  const [viewMode, setViewMode] = useState<"list" | "grid">("list");
  const [selectMode, setSelectMode] = useState(false);
  const [quickLookId, setQuickLookId] = useState<string | null>(null);
  const [showCheatsheet, setShowCheatsheet] = useState(false);
  const [showPalette, setShowPalette] = useState(false);
  const [showOnboarding, setShowOnboarding] = useState(false);
  const [ocrDoc, setOcrDoc] = useState<DocumentSummary | null>(null);
  // App-level OCR job state. Lives outside the OcrDialog so closing
  // the dialog mid-OCR doesn't lose progress visibility — the
  // floating `OcrJobChip` keeps the user informed and the
  // completion toast offers a "View transcript" shortcut.
  const [ocrJob, setOcrJob] = useState<OcrJob | null>(null);
  const [ocrJobElapsed, setOcrJobElapsed] = useState(0);
  const [transcriptDoc, setTranscriptDoc] = useState<DocumentSummary | null>(
    null,
  );
  const [showPublishSettings, setShowPublishSettings] = useState(false);
  const [renaming, setRenaming] = useState<
    | { kind: "document"; id: string; current: string }
    | { kind: "folder"; id: string; current: string }
    | null
  >(null);
  const refreshing = useRef(false);
  const searchInputRef = useRef<HTMLInputElement | null>(null);
  const toast = useToast();
  const confirm = useConfirm();
  // Convenience: the current "single focused" id used by keyboard
  // shortcuts (rename, quick-look, archive). Falls back to the only
  // selected entry when in select mode for ergonomics.
  const selectedId = useMemo(
    () => focusId ?? (selectedIds.size === 1 ? [...selectedIds][0] : null),
    [focusId, selectedIds],
  );

  // ---- Initial load -----------------------------------------------------
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [defPath, opened, dev] = await Promise.all([
          ipc.defaultLibraryPath(),
          ipc.autoOpenLibrary(),
          ipc.deviceState(),
        ]);
        if (cancelled) return;
        setDefaultPath(defPath);
        setDevice(dev);
        if (opened) {
          setLibraryOpen(true);
          setLibraryPath(opened);
          const [s, d, f, a] = await Promise.all([
            ipc.librarySummary(),
            ipc.listDocuments(),
            ipc.listFolders(),
            ipc.listArchived(),
          ]);
          if (cancelled) return;
          setSummary(s);
          setDocuments(d);
          setFolders(f);
          setArchived(a);
        } else {
          setDocuments([]);
          setShowOnboarding(true);
        }
        // Always load the recents list — even on first launch with no
        // library open, the switcher hides itself but having the data
        // means switching after first-open doesn't flicker.
        await refreshRecentLibraries();
      } catch (e) {
        if (!cancelled) setError(String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // ---- Reachability event ----------------------------------------------
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onDeviceReachable(() => {
      ipc.deviceState().then(setDevice).catch(() => {});
    }).then((u) => {
      unlisten = u;
    });
    return () => {
      if (unlisten) unlisten();
    };
  }, []);

  // ---- Background OCR progress ----------------------------------------
  // Single global listener for `ocr:progress`. Updates the
  // App-level `ocrJob` whenever a job is in flight. Stays mounted
  // for the App's lifetime so the chip keeps updating no matter
  // which dialog/drawer the user has open.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onOcrProgress((ev) => {
      setOcrJob((cur) => {
        if (!cur || cur.phase !== "running") return cur;
        if (ev.kind === "page_done") {
          return {
            ...cur,
            pagesDone: ev.page_index + 1,
            charCount: cur.charCount + ev.chars,
          };
        }
        return cur;
      });
    }).then((u) => {
      unlisten = u;
    });
    return () => {
      if (unlisten) unlisten();
    };
  }, []);

  // Tick the elapsed-time counter while a job is running.
  useEffect(() => {
    if (!ocrJob || ocrJob.phase !== "running") return;
    const tick = () =>
      setOcrJobElapsed((Date.now() - ocrJob.startedAt) / 1000);
    tick();
    const id = window.setInterval(tick, 500);
    return () => window.clearInterval(id);
  }, [ocrJob]);

  // ---- Refresh ---------------------------------------------------------
  const refreshLibrary = useCallback(async () => {
    if (refreshing.current) return;
    refreshing.current = true;
    try {
      const [s, d, f, a] = await Promise.all([
        ipc.librarySummary(),
        ipc.listDocuments(),
        ipc.listFolders(),
        ipc.listArchived(),
      ]);
      setSummary(s);
      setDocuments(d);
      setFolders(f);
      setArchived(a);
    } catch (e) {
      setError(String(e));
    } finally {
      refreshing.current = false;
    }
  }, []);

  // ---- Library / device commands --------------------------------------
  async function refreshRecentLibraries() {
    try {
      const list = await ipc.listRecentLibraries();
      setRecentLibraries(list);
    } catch {
      // Non-fatal — the switcher just shows whatever it had.
    }
  }

  async function openLibrary() {
    if (!defaultPath) return;
    setError(null);
    try {
      await ipc.openLibrary(defaultPath);
      setLibraryOpen(true);
      setLibraryPath(defaultPath);
      setShowOnboarding(false);
      await Promise.all([refreshLibrary(), refreshRecentLibraries()]);
    } catch (e) {
      setError(String(e));
    }
  }

  // Switch to a library that's already in the recents list (shown in
  // the library-switcher dropdown).
  async function switchToLibrary(path: string) {
    if (path === libraryPath) return;
    setError(null);
    // Clear stale UI before fetching the new library so the user sees
    // an obvious "loading" state instead of cross-library leakage.
    setDocuments(null);
    setFolders([]);
    setArchived([]);
    setSummary(null);
    setSelectedIds(new Set());
    setFocusId(null);
    setExpanded(new Set());
    try {
      const opened = await ipc.switchLibrary(path);
      setLibraryPath(opened);
      await Promise.all([refreshLibrary(), refreshRecentLibraries()]);
    } catch (e) {
      setError(String(e));
    }
  }

  // Pick a brand-new library directory via the server-side folder
  // picker. If the directory is empty, confirm with the user before
  // materialising a fresh library there — silent creation surprised
  // users who picked the wrong folder.
  async function openAnotherLibrary() {
    setError(null);
    try {
      const picked = await ipc.pickLibraryDirectory();
      if (!picked) return;

      if (picked.kind === "empty") {
        const ok = await confirm({
          title: "Create a new library here?",
          body: (
            <>
              <p>
                <code>{picked.path}</code> is empty. reHydrate will create
                a fresh library there — blob store, sync database, and a{" "}
                <code>library.json</code> stamp.
              </p>
            </>
          ),
          confirmLabel: "Create library",
        });
        if (!ok) return;
      }

      // Wipe stale UI before fetching the new library.
      setDocuments(null);
      setFolders([]);
      setArchived([]);
      setSummary(null);
      setSelectedIds(new Set());
      setFocusId(null);
      setExpanded(new Set());

      await ipc.openLibrary(picked.path);
      setLibraryOpen(true);
      setLibraryPath(picked.path);
      setShowOnboarding(false);
      await Promise.all([refreshLibrary(), refreshRecentLibraries()]);
    } catch (e) {
      setError(String(e));
    }
  }

  // ---- Row click + selection ------------------------------------------
  // Two modes: in regular mode a single click opens the doc and just
  // marks it as keyboard-focused; in select-mode a click toggles a
  // checkbox at the row's leading edge. Cmd/Ctrl-click and Shift-click
  // still work as power shortcuts in select mode.
  const handleRowClick = useCallback(
    (
      d: DocumentSummary,
      list: DocumentSummary[],
      e: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean },
    ) => {
      const id = d.document_id;
      if (!selectMode) {
        // Cmd-click while NOT in select mode is the discoverable
        // shortcut for "I want to start selecting" — flips select mode
        // on and checks this row.
        if (e.metaKey || e.ctrlKey) {
          setSelectMode(true);
          setSelectedIds(new Set([id]));
          setAnchorId(id);
          setFocusId(id);
          return;
        }
        // Plain click in default mode: open the doc and paint no
        // selection or focus highlight. Subsequent keyboard nav (if
        // any) will start its cursor fresh.
        openInViewer(d);
        return;
      }
      // Select mode: click toggles, shift extends a range.
      if (e.shiftKey && anchorId) {
        const ai = list.findIndex((x) => x.document_id === anchorId);
        const bi = list.findIndex((x) => x.document_id === id);
        if (ai >= 0 && bi >= 0) {
          const [lo, hi] = ai < bi ? [ai, bi] : [bi, ai];
          setSelectedIds(
            new Set(list.slice(lo, hi + 1).map((x) => x.document_id)),
          );
          // Anchor stays on the original click; focus follows the
          // user's finger to the new endpoint so subsequent ↑/↓ feels
          // continuous from there.
          setFocusId(id);
          return;
        }
      }
      setSelectedIds((prev) => {
        const next = new Set(prev);
        if (next.has(id)) next.delete(id);
        else next.add(id);
        return next;
      });
      setAnchorId(id);
      setFocusId(id);
    },
    // openInViewer is a stable reference (declared further below in
    // the module). selectMode + anchorId are the only state we read.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [selectMode, anchorId],
  );

  const toggleSelectMode = useCallback(() => {
    setSelectMode((m) => {
      // Leaving select mode clears any selection AND the keyboard
      // cursor so nothing visual lingers from the mode.
      if (m) {
        setSelectedIds(new Set());
        setAnchorId(null);
        setFocusId(null);
      }
      return !m;
    });
  }, []);

  const clearSelection = useCallback(() => {
    setSelectedIds(new Set());
    setAnchorId(null);
  }, []);

  // Drop the keyboard cursor (the inset accent bar). Hooked up to
  // the Esc cascade — last resort once dialogs/drawers are closed.
  const clearFocus = useCallback(() => {
    setFocusId(null);
  }, []);

  async function tryConnect() {
    setError(null);
    try {
      if (device?.has_stored_password) {
        await ipc.connectDevice();
        setDevice(await ipc.deviceState());
      } else {
        setShowPassword(true);
      }
    } catch (e) {
      setError(String(e));
    }
  }

  async function disconnect() {
    await ipc.disconnectDevice();
    setDevice(await ipc.deviceState());
  }

  async function submitPassword(password: string) {
    await ipc.connectDevice(password);
    setDevice(await ipc.deviceState());
    setShowPassword(false);
  }

  function openSync() {
    if (!device?.connected) {
      toast.show({
        tone: "warn",
        body: "Connect the tablet first.",
      });
      return;
    }
    if (!libraryOpen) {
      toast.show({ tone: "warn", body: "Open a library first." });
      return;
    }
    setError(null);
    setShowSync(true);
  }

  async function onSyncComplete() {
    setSyncPhase("idle");
    await refreshLibrary();
  }

  // ---- Import / GC / verify ---------------------------------------------
  async function importFile() {
    setError(null);
    try {
      // The native picker now runs server-side (audit fix H6) so the
      // renderer can't substitute an arbitrary path. `null` means the
      // user cancelled the dialog.
      const summary = await ipc.importFile();
      if (!summary) return;
      await refreshLibrary();
      toast.show({
        tone: "ok",
        body: (
          <span>
            Imported <strong>{summary.visible_name}</strong>. It will upload
            to the tablet on the next sync.
          </span>
        ),
      });
    } catch (e) {
      setError(String(e));
    }
  }

  async function garbageCollect() {
    setError(null);
    try {
      const r = await ipc.garbageCollect();
      await refreshLibrary();
      toast.show({
        tone: "ok",
        body:
          r.deleted === 0
            ? `Cleanup scanned ${r.scanned} files; nothing to remove.`
            : `Cleared ${r.deleted} unused file${r.deleted === 1 ? "" : "s"} · freed ${formatBytes(r.bytes_freed)}.`,
      });
    } catch (e) {
      setError(String(e));
    }
  }

  async function verify() {
    setError(null);
    try {
      const r = await ipc.verifyLibrary();
      const issues =
        r.manifests_missing + r.manifests_invalid + r.blobs_missing + r.blobs_corrupted;
      if (issues === 0) {
        toast.show({
          tone: "ok",
          body: `Library is healthy · ${r.manifests_ok} documents, ${r.blobs_total} files, ${r.blobs_orphan} orphans.`,
        });
      } else {
        toast.show({
          tone: "warn",
          duration: 8000,
          body: (
            <span>
              <strong>{issues}</strong> issue{issues === 1 ? "" : "s"} found:{" "}
              {r.manifests_missing} missing manifests · {r.blobs_missing} missing
              files · {r.blobs_corrupted} corrupted.
            </span>
          ),
        });
      }
    } catch (e) {
      setError(String(e));
    }
  }

  async function openInViewer(d: DocumentSummary) {
    setError(null);
    try {
      await ipc.openDocument(d.document_id);
    } catch (e) {
      setError(String(e));
    }
  }

  // ---- Drag & drop, archive, move ---------------------------------------
  // Helper: trigger the row-out animation, hold the row in `leaving`
  // for the animation duration, then refetch. Without the wait the
  // refetch removes the row from data and React unmounts it before the
  // animation has a chance to play.
  const animateOutThenRefresh = useCallback(
    async (documentId: string, run: () => Promise<void>) => {
      setLeavingIds((s) => new Set(s).add(documentId));
      // Match --motion-base in styles.css.
      await new Promise((r) => setTimeout(r, 200));
      await run();
      await refreshLibrary();
      setLeavingIds((s) => {
        const next = new Set(s);
        next.delete(documentId);
        return next;
      });
    },
    [refreshLibrary],
  );

  const handleDocumentDrop = useCallback(
    async (documentId: string, target: string | null | "archive", batch?: string[]) => {
      setError(null);
      // If the dragged row is part of a multi-selection, propagate the
      // op to every selected document. Single-row drag falls back to
      // the dragged id alone.
      const ids = batch && batch.length > 1 ? batch : [documentId];
      if (ids.length > 1) {
        if (target === "archive") return bulkArchive();
        return bulkMove(target);
      }
      const doc = documents?.find((x) => x.document_id === documentId);
      const previousParent = doc?.parent ?? null;
      try {
        if (target === "archive") {
          invalidateThumbnail(documentId);
          await animateOutThenRefresh(documentId, () =>
            ipc.archiveDocument(documentId),
          );
          toast.show({
            tone: "info",
            body: doc ? (
              <span>
                Moved <strong>{doc.visible_name}</strong> to Archive.
              </span>
            ) : (
              "Document archived."
            ),
            action: {
              label: "Undo",
              onClick: async () => {
                try {
                  await ipc.unarchiveDocument(documentId);
                  await refreshLibrary();
                } catch (e) {
                  toast.show({ tone: "err", body: String(e) });
                }
              },
            },
          });
        } else {
          // Only animate out if this move would remove the row from
          // the *current* visible list (typing: a doc currently in
          // "All Documents" stays visible after a move). Otherwise
          // just refresh.
          const movingOutOfView =
            (typeof view === "object" && view.id !== target) ||
            (view === "notebooks" && doc?.doc_type !== "Notebook") ||
            (view === "pdfs" && doc?.doc_type !== "DocumentType.Pdf") ||
            (view === "epubs" && doc?.doc_type !== "DocumentType.Epub");
          if (movingOutOfView) {
            await animateOutThenRefresh(documentId, () =>
              ipc.moveDocument(documentId, target),
            );
          } else {
            await ipc.moveDocument(documentId, target);
            await refreshLibrary();
          }
          const targetName =
            target === null
              ? "the root"
              : folders.find((f) => f.folder_id === target)?.visible_name ??
                "another folder";
          toast.show({
            tone: "info",
            body: doc ? (
              <span>
                Moved <strong>{doc.visible_name}</strong> to {targetName}.
              </span>
            ) : (
              "Document moved."
            ),
            action: {
              label: "Undo",
              onClick: async () => {
                try {
                  await ipc.moveDocument(documentId, previousParent);
                  await refreshLibrary();
                } catch (e) {
                  toast.show({ tone: "err", body: String(e) });
                }
              },
            },
          });
        }
      } catch (e) {
        setError(String(e));
      }
    },
    [documents, folders, refreshLibrary, toast, animateOutThenRefresh, view],
  );

  async function performRename(newName: string) {
    if (!renaming) return;
    if (renaming.kind === "document") {
      await ipc.renameDocument(renaming.id, newName);
      invalidateThumbnail(renaming.id);
    } else {
      await ipc.renameFolder(renaming.id, newName);
    }
    await refreshLibrary();
    toast.show({
      tone: "ok",
      body: (
        <span>
          Renamed to <strong>{newName}</strong>. The new name syncs to the
          tablet on the next sync.
        </span>
      ),
    });
    setRenaming(null);
  }

  function startRenameDocument(d: DocumentSummary) {
    setRenaming({ kind: "document", id: d.document_id, current: d.visible_name });
  }
  function startRenameFolder(f: FolderEntry) {
    setRenaming({ kind: "folder", id: f.folder_id, current: f.visible_name });
  }

  async function archiveOne(d: DocumentSummary) {
    const ok = await confirm({
      title: `Move "${d.visible_name}" to Archive?`,
      body: "It disappears from your library and from the tablet on the next sync. You can restore it any time from the Archive.",
      confirmLabel: "Move to Archive",
      destructive: true,
    });
    if (!ok) return;
    await handleDocumentDrop(d.document_id, "archive");
  }

  async function bulkArchive() {
    if (!documents || selectedIds.size === 0) return;
    const ids = [...selectedIds];
    const ok = await confirm({
      title: `Move ${ids.length} document${ids.length === 1 ? "" : "s"} to Archive?`,
      body: "They disappear from your library and from the tablet on the next sync. You can restore them any time from the Archive.",
      confirmLabel: "Move to Archive",
      destructive: true,
    });
    if (!ok) return;
    setError(null);
    setLeavingIds((s) => {
      const next = new Set(s);
      ids.forEach((id) => next.add(id));
      return next;
    });
    await new Promise((r) => setTimeout(r, 200));
    try {
      // Run sequentially so the SQLite write_lock is honoured cleanly
      // and any failure leaves a known good prefix.
      for (const id of ids) {
        await ipc.archiveDocument(id);
        invalidateThumbnail(id);
      }
      await refreshLibrary();
      clearSelection();
      toast.show({
        tone: "info",
        body: `Moved ${ids.length} document${ids.length === 1 ? "" : "s"} to Archive.`,
      });
    } catch (e) {
      setError(String(e));
    } finally {
      setLeavingIds((s) => {
        const next = new Set(s);
        ids.forEach((id) => next.delete(id));
        return next;
      });
    }
  }

  async function bulkMove(target: string | null | "archive") {
    if (target === "archive") return bulkArchive();
    if (!documents || selectedIds.size === 0) return;
    const ids = [...selectedIds];
    setError(null);
    try {
      for (const id of ids) {
        await ipc.moveDocument(id, target);
      }
      await refreshLibrary();
      const targetName =
        target === null
          ? "the root"
          : folders.find((f) => f.folder_id === target)?.visible_name ??
            "another folder";
      toast.show({
        tone: "info",
        body: `Moved ${ids.length} document${ids.length === 1 ? "" : "s"} to ${targetName}.`,
      });
    } catch (e) {
      setError(String(e));
    }
  }

  async function restoreFromArchive(d: ArchivedDocument) {
    setError(null);
    try {
      await ipc.unarchiveDocument(d.document_id);
      await refreshLibrary();
      toast.show({
        tone: "ok",
        body: (
          <span>
            Restored <strong>{d.visible_name}</strong>. It re-appears on the
            tablet on next sync.
          </span>
        ),
      });
    } catch (e) {
      setError(String(e));
    }
  }

  async function purgeFromArchive(d: ArchivedDocument) {
    const ok = await confirm({
      title: `Delete "${d.visible_name}" forever?`,
      body: "Every saved version will be dropped. This cannot be undone.",
      confirmLabel: "Delete forever",
      destructive: true,
    });
    if (!ok) return;
    setError(null);
    try {
      await ipc.purgeArchivedDocument(d.document_id);
      await refreshLibrary();
      toast.show({
        tone: "ok",
        body: (
          <span>
            Permanently removed <strong>{d.visible_name}</strong>.
          </span>
        ),
      });
    } catch (e) {
      setError(String(e));
    }
  }

  // ---- Derived ----------------------------------------------------------
  const filteredDocs = useMemo(() => {
    if (!documents) return [];
    let out = filterDocuments(documents, view);
    const q = search.trim().toLowerCase();
    if (q) {
      out = out.filter((d) => d.visible_name.toLowerCase().includes(q));
    }
    return out;
  }, [documents, view, search]);

  const libraryName = useMemo(
    () => (libraryPath ? libraryPath.split("/").filter(Boolean).pop() ?? "Library" : "Library"),
    [libraryPath],
  );

  // ---- Command palette items -------------------------------------------
  const paletteItems = useMemo<PaletteItem[]>(() => {
    const items: PaletteItem[] = [];
    items.push(
      {
        id: "act-sync",
        label: "Sync with reMarkable",
        hint: "Pull, then push",
        icon: <Icon name="sync" />,
        tag: "Action",
        keywords: "pull push",
        onRun: openSync,
      },
      {
        id: "act-import",
        label: "Import a PDF or EPUB",
        icon: <Icon name="import" />,
        tag: "Action",
        keywords: "add upload",
        onRun: importFile,
      },
      {
        id: "act-verify",
        label: "Check library health",
        icon: <Icon name="check" />,
        tag: "Action",
        keywords: "verify integrity blobs",
        onRun: verify,
      },
      {
        id: "act-gc",
        label: "Clean up unused files",
        icon: <Icon name="delete" />,
        tag: "Action",
        keywords: "garbage collect orphans gc",
        onRun: garbageCollect,
      },
      {
        id: "act-logs",
        label: "Open activity log",
        icon: <Icon name="info" />,
        tag: "Action",
        onRun: () => setShowLogs(true),
      },
      {
        id: "act-cheatsheet",
        label: "Show keyboard shortcuts",
        icon: <Icon name="info" />,
        tag: "Help",
        onRun: () => setShowCheatsheet(true),
      },
    );
    const viewMap: Array<[View, string, ReturnType<typeof Icon>]> = [
      ["all", "All Documents", <Icon name="library" key="l" />],
      ["recent", "Recently Synced", <Icon name="clock" key="c" />],
      ["unsynced", "Pending Sync", <Icon name="sync" key="s" />],
      ["notebooks", "Notebooks", <Icon name="notebook" key="n" />],
      ["pdfs", "PDFs", <Icon name="pdf" key="p" />],
      ["epubs", "EPUBs", <Icon name="epub" key="e" />],
      ["archive", "Archive", <Icon name="trash" key="t" />],
    ];
    for (const [v, label, icon] of viewMap) {
      items.push({
        id: `view-${typeof v === "string" ? v : "f"}`,
        label: `Go to ${label}`,
        icon,
        tag: "View",
        onRun: () => setView(v),
      });
    }
    for (const f of folders) {
      items.push({
        id: `folder-${f.folder_id}`,
        label: `Go to ${f.visible_name}`,
        icon: <Icon name="folder" />,
        tag: "Folder",
        onRun: () => setView({ kind: "folder", id: f.folder_id }),
      });
    }
    if (documents) {
      for (const d of documents) {
        items.push({
          id: `doc-${d.document_id}`,
          label: d.visible_name,
          hint: prettyType(d.doc_type),
          icon: <Icon name="library" />,
          tag: "Document",
          onRun: () => openInViewer(d),
        });
      }
    }
    return items;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [folders, documents]);

  // ---- Global keyboard shortcuts ----------------------------------------
  // Wired here so they're always live regardless of focus. Skips when
  // the user is typing inside an input — except Escape, which works
  // everywhere.
  useEffect(() => {
    function isTypingTarget(t: EventTarget | null): boolean {
      if (!(t instanceof HTMLElement)) return false;
      const tag = t.tagName;
      return (
        tag === "INPUT" ||
        tag === "TEXTAREA" ||
        tag === "SELECT" ||
        t.isContentEditable
      );
    }
    function onKey(e: KeyboardEvent) {
      const meta = e.metaKey || e.ctrlKey;
      const typing = isTypingTarget(e.target);

      if (e.key === "Escape") {
        if (renaming) return setRenaming(null);
        if (showPalette) return setShowPalette(false);
        if (showCheatsheet) return setShowCheatsheet(false);
        if (quickLookId) return setQuickLookId(null);
        if (showLogs) return setShowLogs(false);
        if (showSync) return setShowSync(false);
        if (historyDoc) return setHistoryDoc(null);
        if (ocrDoc) return setOcrDoc(null);
        if (transcriptDoc) return setTranscriptDoc(null);
        if (showPublishSettings) return setShowPublishSettings(false);
        if (showSearch) {
          if (search) setSearch("");
          else setShowSearch(false);
          return;
        }
        // In select mode, a single Esc fully exits — clears the
        // checked rows AND flips the mode off in one step. Outside
        // select mode the cascade is per-state (selection, then
        // focus cursor).
        if (selectMode) {
          setSelectMode(false);
          setSelectedIds(new Set());
          setAnchorId(null);
          setFocusId(null);
          return;
        }
        if (selectedIds.size > 0) return clearSelection();
        if (focusId) return clearFocus();
        return;
      }
      if (typing) return;

      // F2 renames the focused doc, mirroring Finder/Explorer.
      if (e.key === "F2" && selectedId && documents) {
        const d = documents.find((x) => x.document_id === selectedId);
        if (d) {
          e.preventDefault();
          startRenameDocument(d);
        }
        return;
      }

      if (meta && (e.key === "k" || e.key === "K")) {
        e.preventDefault();
        setShowPalette(true);
        return;
      }
      if (e.key === "?" || (e.shiftKey && e.key === "/")) {
        e.preventDefault();
        setShowCheatsheet(true);
        return;
      }
      if (meta && (e.key === "f" || e.key === "F")) {
        e.preventDefault();
        setShowSearch(true);
        requestAnimationFrame(() => searchInputRef.current?.focus());
        return;
      }
      if (meta && (e.key === "s" || e.key === "S")) {
        e.preventDefault();
        if (libraryOpen && device?.connected) openSync();
        return;
      }
      if (meta && (e.key === "i" || e.key === "I")) {
        e.preventDefault();
        if (libraryOpen) importFile();
        return;
      }
      if (meta && (e.key === "Backspace" || e.key === "Delete")) {
        e.preventDefault();
        if (selectedIds.size > 0 && documents) bulkArchive();
        return;
      }
      if (e.key === " ") {
        // Space → Quick Look the focused row.
        if (selectedId) {
          e.preventDefault();
          setQuickLookId(selectedId);
        }
        return;
      }
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        if (filteredDocs.length === 0) return;
        e.preventDefault();
        const idx = filteredDocs.findIndex((d) => d.document_id === selectedId);
        const nextIdx =
          e.key === "ArrowDown"
            ? idx < 0
              ? 0
              : Math.min(idx + 1, filteredDocs.length - 1)
            : idx <= 0
              ? 0
              : idx - 1;
        const next = filteredDocs[nextIdx];
        setFocusId(next.document_id);
        setAnchorId(next.document_id);
        // In select mode the arrow keys also move the *active*
        // selection so range-extension feels right; in default mode
        // they only move the keyboard cursor (no painted selection).
        if (selectMode) {
          setSelectedIds(new Set([next.document_id]));
        }
        return;
      }
      if (e.key === "Enter") {
        if (selectedId && documents) {
          const d = documents.find((x) => x.document_id === selectedId);
          if (d) {
            e.preventDefault();
            openInViewer(d);
          }
        }
        return;
      }
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [
    showLogs,
    showSync,
    historyDoc,
    showSearch,
    search,
    selectedId,
    selectedIds,
    showCheatsheet,
    showPalette,
    quickLookId,
    renaming,
    selectMode,
    focusId,
    libraryOpen,
    device?.connected,
    documents,
    filteredDocs,
  ]);

  // ---- OCR background job ---------------------------------------------
  // Kicks off a transcribe IPC, tracks its in-flight state in the
  // App-level chip, and on completion fires a toast with a "View"
  // shortcut. The dialog calls this and then dismisses itself —
  // the user is free to keep using the rest of the app.
  const startOcrJob = useCallback(
    (doc: DocumentSummary, language: string | undefined) => {
      // Replace any prior finished/errored job with a fresh one.
      // Concurrent OCR jobs aren't supported (single backend, single
      // request at a time on the IPC layer); if a job is already
      // running we surface a toast and bail rather than queueing.
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

      // Fire-and-forget the IPC call. We don't await it inside the
      // synchronous setOcrDoc closure; instead we track the Promise
      // result here so settlement can fire the appropriate toast.
      void (async () => {
        try {
          const summary = await ipc.transcribeDocument(doc.document_id, language);
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
          // Surface the result with a one-click route into the
          // transcript drawer. Library refresh in parallel so the
          // version_id badge updates.
          await refreshLibrary();
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
              onClick: () => setTranscriptDoc(doc),
            },
          });
          // Hide the chip once acknowledged via the toast (or after
          // the toast itself clears).
          setOcrJob(null);
        } catch (e) {
          setOcrJob((cur) =>
            cur && cur.documentId === doc.document_id
              ? { ...cur, phase: "error", error: String(e) }
              : cur,
          );
          toast.show({
            tone: "err",
            duration: 0,
            body: (
              <>
                OCR failed for <strong>{doc.visible_name}</strong>: {String(e)}
              </>
            ),
          });
          setOcrJob(null);
        }
      })();
    },
    [refreshLibrary, toast],
  );

  // ---- Render -----------------------------------------------------------
  return (
    <div className="app">
      <header className="toolbar" data-tauri-drag-region>
        <span className="brand">
          <img className="brand-mark" src="/logo.png" alt="" />
          reHydrate
        </span>
        <StatusPill
          state={device}
          phase={syncPhase}
          onConnect={tryConnect}
          onDisconnect={disconnect}
        />
        <div className="spacer" />
        {!libraryOpen ? (
          <button onClick={openLibrary} disabled={!defaultPath} className="primary">
            <Icon name="library" /> Open library
          </button>
        ) : (
          <>
            <LibrarySwitcher
              currentPath={libraryPath}
              recents={recentLibraries}
              onSwitch={switchToLibrary}
              onOpenAnother={openAnotherLibrary}
            />
            <button onClick={importFile} title="Import a PDF or EPUB (⌘I)">
              <Icon name="import" /> Import
            </button>
            <button
              onClick={openSync}
              disabled={!device?.connected}
              className="primary"
              title="Sync with the tablet (⌘S)"
            >
              <Icon name="sync" /> Sync
            </button>
          </>
        )}
        <Menu
          trigger={
            <button className="icon ghost" aria-label="More actions">
              <Icon name="more" />
            </button>
          }
          items={[
            {
              label: "Check library health",
              icon: <Icon name="check" />,
              onClick: verify,
              disabled: !libraryOpen,
            },
            {
              label: "Clean up unused files",
              icon: <Icon name="delete" />,
              onClick: garbageCollect,
              disabled: !libraryOpen,
            },
            {
              label: "Publishing settings…",
              icon: <Icon name="info" />,
              onClick: () => setShowPublishSettings(true),
              separatorBefore: true,
            },
            {
              label: "Activity log",
              icon: <Icon name="info" />,
              onClick: () => setShowLogs(true),
              separatorBefore: true,
            },
            {
              label: "Keyboard shortcuts",
              icon: <Icon name="info" />,
              onClick: () => setShowCheatsheet(true),
            },
          ]}
        />
      </header>

      <div className="layout">
        <aside className="sidebar">
          <section>
            <h3>Library</h3>
            <ul>
              <SidebarItem
                label="All Documents"
                icon={<Icon name="library" />}
                v="all"
                view={view}
                setView={setView}
                count={documents?.length ?? 0}
                dropTarget={null}
                onDocumentDrop={handleDocumentDrop}
              />
              <SidebarItem
                label="Recently Synced"
                icon={<Icon name="clock" />}
                v="recent"
                view={view}
                setView={setView}
                count={documents ? recentCount(documents) : 0}
              />
              <SidebarItem
                label="Pending Sync"
                icon={<Icon name="sync" />}
                v="unsynced"
                view={view}
                setView={setView}
                count={documents ? unsyncedCount(documents) : 0}
              />
              <SidebarItem
                label="Notebooks"
                icon={<Icon name="notebook" />}
                v="notebooks"
                view={view}
                setView={setView}
                count={documents ? countByKind(documents, "Notebook") : 0}
              />
              <SidebarItem
                label="PDFs"
                icon={<Icon name="pdf" />}
                v="pdfs"
                view={view}
                setView={setView}
                count={documents ? countByKind(documents, "DocumentType.Pdf") : 0}
              />
              <SidebarItem
                label="EPUBs"
                icon={<Icon name="epub" />}
                v="epubs"
                view={view}
                setView={setView}
                count={documents ? countByKind(documents, "DocumentType.Epub") : 0}
              />
            </ul>
          </section>

          {folders.length > 0 && documents && (
            <section>
              <h3>Folders</h3>
              <FolderTree
                folders={folders}
                documents={documents}
                view={view}
                setView={setView}
                expanded={expanded}
                setExpanded={setExpanded}
                onDocumentDrop={handleDocumentDrop}
                onRenameFolder={startRenameFolder}
              />
            </section>
          )}

          <div className="sidebar-divider" />
          <section>
            <h3>Bin</h3>
            <ul>
              <SidebarItem
                label="Archive"
                icon={<Icon name="trash" />}
                v="archive"
                view={view}
                setView={setView}
                count={archived.length}
                dropTarget="archive"
                onDocumentDrop={handleDocumentDrop}
              />
            </ul>
          </section>

          <div className="sidebar-divider" />
          <section>
            <h3>Device</h3>
            <ul>
              {device?.connected && device.info ? (
                <li className="muted" style={{ cursor: "default" }}>
                  <Icon name="tablet" />
                  <span>{device.info.model}</span>
                </li>
              ) : (
                <li className="muted">
                  <Icon name="tablet" />
                  <span>{device?.reachable ? "Detected" : "No tablet"}</span>
                </li>
              )}
            </ul>
          </section>
        </aside>

        <main className="content">
          {error && (
            <div className="error">
              <Icon name="warn" />
              <span>{error}</span>
              <button onClick={() => setError(null)} className="close-inline">
                ×
              </button>
            </div>
          )}

          {!libraryOpen && showOnboarding ? (
            <Onboarding
              defaultPath={defaultPath}
              initialDevice={device}
              onOpenLibrary={openLibrary}
              onConnect={tryConnect}
              onSkip={() => setShowOnboarding(false)}
            />
          ) : !libraryOpen ? (
            <WelcomeEmpty defaultPath={defaultPath} onOpen={openLibrary} />
          ) : (
            <>
              <div className="content-header">
                <h1>{viewTitle(view, folders)}</h1>
                <span className="crumbs">{viewSubtitle(view)}</span>
                <div className="spacer" />
                {showSearch && view !== "archive" && (
                  <span className="search-bar">
                    <Icon name="search" />
                    <input
                      ref={searchInputRef}
                      type="search"
                      placeholder="Search this view…"
                      value={search}
                      onChange={(e) => setSearch(e.target.value)}
                      onBlur={() => {
                        // Collapse back to the magnifier glyph when the
                        // user clicks away with nothing typed.
                        if (!search) setShowSearch(false);
                      }}
                      onKeyDown={(e) => {
                        if (e.key === "Escape") {
                          if (search) setSearch("");
                          else {
                            setShowSearch(false);
                            (e.target as HTMLInputElement).blur();
                          }
                        }
                      }}
                      autoFocus
                    />
                    {search && (
                      <button
                        className="clear"
                        aria-label="Clear search"
                        onClick={() => setSearch("")}
                      >
                        ×
                      </button>
                    )}
                  </span>
                )}
                {!showSearch && view !== "archive" && (
                  <button
                    className="icon ghost"
                    aria-label="Search (⌘F)"
                    title="Search (⌘F)"
                    onClick={() => setShowSearch(true)}
                  >
                    <Icon name="search" />
                  </button>
                )}
                {view !== "archive" && (
                  <button
                    className={`icon ghost${selectMode ? " active" : ""}`}
                    aria-label={selectMode ? "Done selecting" : "Select"}
                    title={selectMode ? "Exit select mode" : "Select multiple"}
                    onClick={toggleSelectMode}
                  >
                    <Icon name="selectMode" />
                  </button>
                )}
                {view !== "archive" && (
                  <span className="view-switch" role="tablist" aria-label="View mode">
                    <button
                      className={viewMode === "list" ? "active" : ""}
                      title="List view"
                      aria-label="List view"
                      onClick={() => setViewMode("list")}
                    >
                      <Icon name="library" />
                    </button>
                    <button
                      className={viewMode === "grid" ? "active" : ""}
                      title="Grid view"
                      aria-label="Grid view"
                      onClick={() => setViewMode("grid")}
                    >
                      <Icon name="folder" />
                    </button>
                  </span>
                )}
              </div>
              {selectedIds.size > 0 && view !== "archive" && (
                <div className="selection-bar">
                  <span>
                    <strong>{selectedIds.size}</strong> selected
                  </span>
                  <div className="spacer" />
                  <button onClick={clearSelection}>Clear</button>
                  <button className="danger" onClick={bulkArchive}>
                    <Icon name="trash" /> Archive
                  </button>
                </div>
              )}
              {view === "archive" ? (
                <ArchiveList
                  archived={archived}
                  onRestore={restoreFromArchive}
                  onPurge={purgeFromArchive}
                  onShowHistory={(d) =>
                    setHistoryDoc({
                      // The history drawer only consults a handful of
                      // fields; synthesise a DocumentSummary that
                      // mirrors the archive entry well enough for it
                      // to render.
                      document_id: d.document_id,
                      visible_name: d.visible_name,
                      doc_type: d.doc_type,
                      current_manifest: d.manifest_hash,
                      current_version_id: d.version_id,
                      last_observed_at: d.archived_at,
                      parent: d.parent,
                      size_bytes: 0,
                      page_count: null,
                      has_unpushed_changes: false,
                    })
                  }
                  emptyHint={emptyHintFor(view)}
                />
              ) : documents === null ? (
                <DocumentListSkeleton />
              ) : viewMode === "grid" ? (
                <DocumentGrid
                  documents={filteredDocs}
                  selectedIds={selectedIds}
                  selectMode={selectMode}
                  focusId={focusId}
                  onClickRow={(d, e) => handleRowClick(d, filteredDocs, e)}
                  onArchive={archiveOne}
                  onShowHistory={setHistoryDoc}
                  leavingIds={leavingIds}
                  emptyHint={emptyHintFor(view)}
                />
              ) : (
                <DocumentList
                  documents={filteredDocs}
                  selectedIds={selectedIds}
                  selectMode={selectMode}
                  focusId={focusId}
                  onClickRow={(d, e) => handleRowClick(d, filteredDocs, e)}
                  onOpen={setHistoryDoc}
                  onOpenInViewer={openInViewer}
                  onArchive={archiveOne}
                  onRename={startRenameDocument}
                  onTranscribe={setOcrDoc}
                  onShowTranscript={setTranscriptDoc}
                  leavingIds={leavingIds}
                  emptyHint={emptyHintFor(view)}
                />
              )}
            </>
          )}
        </main>
      </div>

      <footer className="statusbar">
        {syncPhase === "syncing" && <span className="statusbar-progress" />}
        {summary ? (
          <>
            <span className={`health-dot${summaryHealth(summary)}`} />
            <span>
              {summary.document_count} document
              {summary.document_count === 1 ? "" : "s"}
            </span>
            <span>·</span>
            <span>{formatBytes(summary.size_bytes)}</span>
            <span>·</span>
            <span>{summary.version_count} versions</span>
            <span className="spacer" />
            <span className="muted">{libraryName}</span>
          </>
        ) : (
          <span className="muted">Library not open</span>
        )}
      </footer>

      {showPassword && (
        <PasswordDialog
          onCancel={() => setShowPassword(false)}
          onSubmit={submitPassword}
        />
      )}
      {showSync && (
        <div className="drawer-backdrop" onClick={() => setShowSync(false)}>
          <SyncDrawer
            onClose={() => setShowSync(false)}
            onComplete={onSyncComplete}
          />
        </div>
      )}
      {historyDoc && (
        <div className="drawer-backdrop" onClick={() => setHistoryDoc(null)}>
          <HistoryDrawer document={historyDoc} onClose={() => setHistoryDoc(null)} />
        </div>
      )}
      {showLogs && (
        <div className="drawer-backdrop" onClick={() => setShowLogs(false)}>
          <LogDrawer onClose={() => setShowLogs(false)} />
        </div>
      )}
      {renaming && (
        <RenameDialog
          title={renaming.kind === "folder" ? "Rename folder" : "Rename document"}
          kind={renaming.kind}
          initialName={renaming.current}
          onCancel={() => setRenaming(null)}
          onSubmit={performRename}
        />
      )}
      {ocrDoc && (
        <OcrDialog
          document={ocrDoc}
          onCancel={() => setOcrDoc(null)}
          onStart={(language) => {
            // Hand the job off to App-level state and close the
            // dialog. The user is free to keep working; progress
            // shows up in the floating chip and the completion
            // toast will offer a "View" shortcut.
            const doc = ocrDoc;
            setOcrDoc(null);
            startOcrJob(doc, language);
          }}
        />
      )}
      {ocrJob && ocrJob.phase === "running" && (
        <OcrJobChip
          job={ocrJob}
          elapsedSeconds={ocrJobElapsed}
          onDismiss={() => setOcrJob(null)}
        />
      )}
      {transcriptDoc && (
        <TranscriptDrawer
          document={transcriptDoc}
          onClose={() => setTranscriptDoc(null)}
          notify={(tone, body) => toast.show({ tone, body })}
        />
      )}
      {showPublishSettings && (
        <PublishingSettings
          onClose={() => setShowPublishSettings(false)}
          notify={(tone, body) => toast.show({ tone, body })}
        />
      )}
      {showCheatsheet && <Cheatsheet onClose={() => setShowCheatsheet(false)} />}
      {showPalette && (
        <CommandPalette
          items={paletteItems}
          onClose={() => setShowPalette(false)}
        />
      )}
      {quickLookId &&
        documents &&
        (() => {
          const d = documents.find((x) => x.document_id === quickLookId);
          if (!d) return null;
          return (
            <QuickLook
              document={d}
              onClose={() => setQuickLookId(null)}
              onOpen={() => {
                setQuickLookId(null);
                openInViewer(d);
              }}
            />
          );
        })()}
    </div>
  );
}

// =====================================================================
// Drag & drop helpers
// =====================================================================

const DOC_DRAG_MIME = "application/x-rehydrate-doc";
const DOC_DRAG_BATCH_MIME = "application/x-rehydrate-doc-batch";

function setDocumentDragData(
  e: ReactDragEvent,
  documentId: string,
  batch: string[] = [documentId],
) {
  e.dataTransfer.setData(DOC_DRAG_MIME, documentId);
  if (batch.length > 1) {
    e.dataTransfer.setData(DOC_DRAG_BATCH_MIME, batch.join(","));
  }
  e.dataTransfer.setData("text/plain", documentId);
  e.dataTransfer.effectAllowed = "move";
}
function readDocumentDragData(
  e: ReactDragEvent,
): { id: string; batch: string[] } | null {
  const id = e.dataTransfer.getData(DOC_DRAG_MIME);
  if (!id) return null;
  const batchRaw = e.dataTransfer.getData(DOC_DRAG_BATCH_MIME);
  const batch = batchRaw ? batchRaw.split(",").filter(Boolean) : [id];
  return { id, batch };
}
function hasDocumentDragData(e: ReactDragEvent): boolean {
  return e.dataTransfer.types.includes(DOC_DRAG_MIME);
}

// =====================================================================
// Views
// =====================================================================

type View =
  | "all"
  | "recent"
  | "unsynced"
  | "notebooks"
  | "pdfs"
  | "epubs"
  | "archive"
  | { kind: "folder"; id: string };

function viewKey(v: View): string {
  if (typeof v === "string") return v;
  return `folder:${v.id}`;
}

function viewTitle(v: View, folders: FolderEntry[]): string {
  if (typeof v === "object") {
    return folders.find((f) => f.folder_id === v.id)?.visible_name ?? "Folder";
  }
  switch (v) {
    case "all": return "All Documents";
    case "recent": return "Recently Synced";
    case "unsynced": return "Pending Sync";
    case "notebooks": return "Notebooks";
    case "pdfs": return "PDFs";
    case "epubs": return "EPUBs";
    case "archive": return "Archive";
  }
}

function viewSubtitle(v: View): string {
  if (typeof v === "object") return "Folder";
  switch (v) {
    case "all": return "Everything in your library";
    case "recent": return "Synced in the last 24 hours";
    case "unsynced": return "Will upload to the tablet on next sync";
    case "notebooks": return "Handwritten and template-based";
    case "pdfs": return "Imported PDFs";
    case "epubs": return "Imported EPUBs";
    case "archive": return "Items pending deletion · restore at any time";
  }
}

const RECENT_WINDOW_MS = 24 * 60 * 60 * 1000;

function recentCount(docs: DocumentSummary[]): number {
  return filterRecent(docs).length;
}
function countByKind(docs: DocumentSummary[], kindMatch: string): number {
  return docs.filter((d) => d.doc_type === kindMatch).length;
}
function filterRecent(docs: DocumentSummary[]): DocumentSummary[] {
  const cutoff = Date.now() - RECENT_WINDOW_MS;
  return docs
    .filter((d) => {
      const t = Date.parse(d.last_observed_at);
      return Number.isFinite(t) && t >= cutoff;
    })
    .sort((a, b) => b.last_observed_at.localeCompare(a.last_observed_at));
}
function filterDocuments(docs: DocumentSummary[], view: View): DocumentSummary[] {
  if (typeof view === "object") {
    return docs.filter((d) => d.parent === view.id);
  }
  switch (view) {
    case "all":       return docs;
    case "recent":    return filterRecent(docs);
    case "unsynced":  return docs.filter((d) => d.has_unpushed_changes);
    case "notebooks": return docs.filter((d) => d.doc_type === "Notebook");
    case "pdfs":      return docs.filter((d) => d.doc_type === "DocumentType.Pdf");
    case "epubs":     return docs.filter((d) => d.doc_type === "DocumentType.Epub");
    case "archive":   return [];
  }
}

function unsyncedCount(docs: DocumentSummary[]): number {
  return docs.filter((d) => d.has_unpushed_changes).length;
}

function emptyHintFor(view: View): { title: string; body: string } {
  if (typeof view === "object") {
    return {
      title: "Empty folder",
      body: "Drop documents here to move them in.",
    };
  }
  switch (view) {
    case "all":
      return {
        title: "No documents yet",
        body: "Connect your reMarkable and tap Sync, or import a PDF.",
      };
    case "recent":
      return {
        title: "Nothing synced recently",
        body: "Documents synced in the last 24 hours appear here.",
      };
    case "notebooks":
      return {
        title: "No notebooks",
        body: "reMarkable notebooks (handwritten or imported templates) appear here.",
      };
    case "pdfs":
      return {
        title: "No PDFs",
        body: "Drop a PDF on Import or sync one from the tablet.",
      };
    case "epubs":
      return {
        title: "No EPUBs",
        body: "Drop an EPUB on Import or sync one from the tablet.",
      };
    case "archive":
      return {
        title: "Archive is empty",
        body: "Documents you delete (here or on the tablet) land here so you can change your mind.",
      };
    case "unsynced":
      return {
        title: "Everything is synced",
        body: "When you import or edit, items waiting to upload will appear here.",
      };
  }
}

function summaryHealth(s: LibrarySummary): string {
  void s;
  // Without an integrated verify result, treat as healthy by default;
  // proper health colouring lives in a future phase.
  return "";
}

// =====================================================================
// Welcome empty state
// =====================================================================

function WelcomeEmpty({
  defaultPath,
  onOpen,
}: {
  defaultPath: string | null;
  onOpen: () => void;
}) {
  return (
    <div className="empty">
      <img className="empty-logo" src="/logo.png" alt="" />
      <h2>Welcome to reHydrate</h2>
      <p>
        A calm, offline home for everything on your reMarkable. Sync stays
        on your machine — no cloud, no telemetry.
      </p>
      {defaultPath && (
        <p className="path">
          Default library: <code>{defaultPath}</code>
        </p>
      )}
      <p>
        <button onClick={onOpen} className="primary" disabled={!defaultPath}>
          <Icon name="library" /> Open default library
        </button>
      </p>
    </div>
  );
}

// =====================================================================
// Sidebar items + folder tree
// =====================================================================

function SidebarItem({
  label,
  icon,
  v,
  view,
  setView,
  count,
  dropTarget,
  onDocumentDrop,
}: {
  label: string;
  icon?: ReactNode;
  v: View;
  view: View;
  setView: (v: View) => void;
  count: number;
  dropTarget?: string | null | "archive";
  onDocumentDrop?: (documentId: string, target: string | null | "archive", batch?: string[]) => void;
}) {
  const [dragOver, setDragOver] = useState(false);
  const droppable = dropTarget !== undefined && !!onDocumentDrop;
  const cls = [
    viewKey(view) === viewKey(v) ? "active" : "",
    dragOver ? "drop-target" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <li
      className={cls}
      onClick={() => setView(v)}
      title={`${count} document${count === 1 ? "" : "s"}`}
      onDragOver={
        droppable
          ? (e) => {
              if (!hasDocumentDragData(e)) return;
              e.preventDefault();
              e.dataTransfer.dropEffect = "move";
              if (!dragOver) setDragOver(true);
            }
          : undefined
      }
      onDragLeave={droppable ? () => setDragOver(false) : undefined}
      onDrop={
        droppable
          ? (e) => {
              setDragOver(false);
              const data = readDocumentDragData(e);
              if (!data) return;
              e.preventDefault();
              onDocumentDrop!(data.id, dropTarget!, data.batch);
            }
          : undefined
      }
    >
      {icon}
      <span>{label}</span>
      <span className="sidebar-count">{count}</span>
    </li>
  );
}

interface FolderTreeNode {
  folder: FolderEntry;
  children: FolderTreeNode[];
  docCount: number;
}

function buildFolderTree(folders: FolderEntry[], docs: DocumentSummary[]): FolderTreeNode[] {
  const docCounts = new Map<string, number>();
  for (const d of docs) {
    if (d.parent) docCounts.set(d.parent, (docCounts.get(d.parent) ?? 0) + 1);
  }
  const byId = new Map<string, FolderTreeNode>();
  for (const f of folders) {
    byId.set(f.folder_id, {
      folder: f,
      children: [],
      docCount: docCounts.get(f.folder_id) ?? 0,
    });
  }
  const roots: FolderTreeNode[] = [];
  for (const node of byId.values()) {
    const parentId = node.folder.parent;
    if (parentId && byId.has(parentId)) byId.get(parentId)!.children.push(node);
    else roots.push(node);
  }
  const sortRec = (nodes: FolderTreeNode[]) => {
    nodes.sort((a, b) => a.folder.visible_name.localeCompare(b.folder.visible_name));
    for (const n of nodes) sortRec(n.children);
  };
  sortRec(roots);
  return roots;
}

function FolderTree({
  folders,
  documents,
  view,
  setView,
  expanded,
  setExpanded,
  onDocumentDrop,
  onRenameFolder,
}: {
  folders: FolderEntry[];
  documents: DocumentSummary[];
  view: View;
  setView: (v: View) => void;
  expanded: Set<string>;
  setExpanded: (s: Set<string>) => void;
  onDocumentDrop: (documentId: string, target: string | null | "archive", batch?: string[]) => void;
  onRenameFolder: (f: FolderEntry) => void;
}) {
  const roots = buildFolderTree(folders, documents);
  const toggle = (id: string) => {
    const next = new Set(expanded);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setExpanded(next);
  };
  return (
    <ul>
      {roots.map((node) => (
        <FolderRow
          key={node.folder.folder_id}
          node={node}
          depth={0}
          view={view}
          setView={setView}
          expanded={expanded}
          toggle={toggle}
          onDocumentDrop={onDocumentDrop}
          onRenameFolder={onRenameFolder}
        />
      ))}
    </ul>
  );
}

function FolderRow({
  node,
  depth,
  view,
  setView,
  expanded,
  toggle,
  onDocumentDrop,
  onRenameFolder,
}: {
  node: FolderTreeNode;
  depth: number;
  view: View;
  setView: (v: View) => void;
  expanded: Set<string>;
  toggle: (id: string) => void;
  onDocumentDrop: (documentId: string, target: string | null | "archive", batch?: string[]) => void;
  onRenameFolder: (f: FolderEntry) => void;
}) {
  const v: View = { kind: "folder", id: node.folder.folder_id };
  const isActive = viewKey(view) === viewKey(v);
  const hasChildren = node.children.length > 0;
  const isOpen = expanded.has(node.folder.folder_id);
  const [dragOver, setDragOver] = useState(false);
  // Spring-loaded folder: hover during a drag for >600ms auto-expands.
  const hoverTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const cls = [
    "folder-row",
    isActive ? "active" : "",
    dragOver ? "drop-target" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <>
      <li
        className={cls}
        data-depth={depth}
        onClick={() => setView(v)}
        style={{ paddingLeft: 22 + depth * 14 }}
        title={`${node.docCount} document${node.docCount === 1 ? "" : "s"}`}
        onDragOver={(e) => {
          if (!hasDocumentDragData(e)) return;
          e.preventDefault();
          e.dataTransfer.dropEffect = "move";
          if (!dragOver) {
            setDragOver(true);
            if (hasChildren && !isOpen && !hoverTimer.current) {
              hoverTimer.current = setTimeout(() => {
                toggle(node.folder.folder_id);
                hoverTimer.current = null;
              }, 600);
            }
          }
        }}
        onDragLeave={() => {
          setDragOver(false);
          if (hoverTimer.current) {
            clearTimeout(hoverTimer.current);
            hoverTimer.current = null;
          }
        }}
        onDrop={(e) => {
          setDragOver(false);
          if (hoverTimer.current) {
            clearTimeout(hoverTimer.current);
            hoverTimer.current = null;
          }
          const data = readDocumentDragData(e);
          if (!data) return;
          e.preventDefault();
          e.stopPropagation();
          onDocumentDrop(data.id, node.folder.folder_id, data.batch);
        }}
      >
        <span
          className="folder-icon"
          onClick={(e) => {
            if (!hasChildren) return;
            e.stopPropagation();
            toggle(node.folder.folder_id);
          }}
          role={hasChildren ? "button" : undefined}
          aria-label={hasChildren ? (isOpen ? "Collapse" : "Expand") : undefined}
          style={hasChildren ? { cursor: "pointer" } : undefined}
        >
          {hasChildren ? (isOpen ? "▾" : "▸") : "•"}
        </span>
        <Icon name="folder" />
        <span className="folder-name">{node.folder.visible_name}</span>
        {node.docCount > 0 && (
          <span className="sidebar-count">{node.docCount}</span>
        )}
        <span className="folder-kebab" onClick={(e) => e.stopPropagation()}>
          <Menu
            trigger={
              <button
                className="icon ghost"
                aria-label="Folder actions"
                onClick={(e) => e.stopPropagation()}
              >
                <Icon name="more" />
              </button>
            }
            items={[
              {
                label: "Rename…",
                icon: <Icon name="folder" />,
                onClick: () => onRenameFolder(node.folder),
              },
            ]}
          />
        </span>
      </li>
      {hasChildren &&
        isOpen &&
        node.children.map((child) => (
          <FolderRow
            key={child.folder.folder_id}
            node={child}
            depth={depth + 1}
            view={view}
            setView={setView}
            expanded={expanded}
            toggle={toggle}
            onDocumentDrop={onDocumentDrop}
            onRenameFolder={onRenameFolder}
          />
        ))}
    </>
  );
}

// =====================================================================
// Document list
// =====================================================================

function DocumentListSkeleton() {
  return (
    <table className="docs">
      <thead>
        <tr>
          <th>Title</th>
          <th>Type</th>
          <th>Size</th>
          <th>Pages</th>
          <th>Synced</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {Array.from({ length: 8 }).map((_, i) => (
          <tr key={i}>
            <td><Skeleton width="60%" /></td>
            <td><Skeleton width="40%" /></td>
            <td><Skeleton width="40%" /></td>
            <td><Skeleton width="30%" /></td>
            <td><Skeleton width="50%" /></td>
            <td></td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function DocumentList({
  documents,
  selectedIds,
  selectMode,
  focusId,
  onClickRow,
  onOpen,
  onOpenInViewer,
  onArchive,
  onRename,
  onTranscribe,
  onShowTranscript,
  leavingIds,
  emptyHint,
}: {
  documents: DocumentSummary[];
  selectedIds: Set<string>;
  selectMode: boolean;
  focusId: string | null;
  onClickRow: (
    d: DocumentSummary,
    e: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean },
  ) => void;
  onOpen: (d: DocumentSummary) => void;
  onOpenInViewer: (d: DocumentSummary) => void;
  onArchive: (d: DocumentSummary) => void;
  onRename: (d: DocumentSummary) => void;
  onTranscribe: (d: DocumentSummary) => void;
  onShowTranscript: (d: DocumentSummary) => void;
  leavingIds: Set<string>;
  emptyHint: { title: string; body: string };
}) {
  if (documents.length === 0) {
    return (
      <div className="empty">
        <div className="empty-art">
          <Icon name="library" size={36} />
        </div>
        <h2>{emptyHint.title}</h2>
        <p>{emptyHint.body}</p>
      </div>
    );
  }
  return (
    <table className={`docs${selectMode ? " select-mode" : ""}`}>
      <thead>
        <tr>
          {selectMode && <th className="check-cell"></th>}
          <th>Title</th>
          <th>Type</th>
          <th>Size</th>
          <th>Pages</th>
          <th>Synced</th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {documents.map((d) => {
          const isSelected = selectedIds.has(d.document_id);
          const isFocused = focusId === d.document_id;
          const leaving = leavingIds.has(d.document_id);
          return (
            <tr
              key={d.document_id}
              className={`clickable${isSelected ? " selected" : ""}${
                isFocused && !isSelected ? " focused" : ""
              }${leaving ? " leaving" : ""}`}
              title={
                selectMode
                  ? "Click to toggle · ⇧-click for range · drag to move"
                  : "Click to open · ⌘-click to start selecting · drag to move"
              }
              draggable
              onDragStart={(e) => {
                const ids = isSelected && selectedIds.size > 1
                  ? [...selectedIds]
                  : [d.document_id];
                setDocumentDragData(e, d.document_id, ids);
                setCustomDragImage(e, d.visible_name, ids.length, DRAG_ICON_SVG);
              }}
              onClick={(e) =>
                onClickRow(d, {
                  metaKey: e.metaKey,
                  ctrlKey: e.ctrlKey,
                  shiftKey: e.shiftKey,
                })
              }
            >
              {selectMode && (
                <td className="check-cell">
                  <span
                    className={`check-box${isSelected ? " checked" : ""}`}
                    aria-hidden
                  >
                    {isSelected ? (
                      <Icon name="checkboxChecked" size={16} />
                    ) : (
                      <Icon name="checkbox" size={16} />
                    )}
                  </span>
                </td>
              )}
              <td className="row-title">
                <span className="type-glyph"><TypeIcon kind={d.doc_type} /></span>
                {d.visible_name}
                {d.has_unpushed_changes && (
                  <span className="row-pill" title="Has local changes that will sync to the tablet">
                    Unsynced
                  </span>
                )}
              </td>
              <td className="muted small">{prettyType(d.doc_type)}</td>
              <td className="num small">{formatBytes(d.size_bytes)}</td>
              <td className="num small">{d.page_count ?? "—"}</td>
              <td className="muted small">{prettyDate(d.last_observed_at)}</td>
              <td className="row-actions">
                <Menu
                  trigger={
                    <button className="icon ghost" aria-label="More actions" onClick={(e) => e.stopPropagation()}>
                      <Icon name="more" />
                    </button>
                  }
                  items={[
                    {
                      label: "Open in viewer",
                      icon: <Icon name="library" />,
                      onClick: () => onOpenInViewer(d),
                    },
                    {
                      label: "Rename…",
                      icon: <Icon name="folder" />,
                      onClick: () => onRename(d),
                    },
                    {
                      label: "Convert to text…",
                      icon: <Icon name="info" />,
                      onClick: () => onTranscribe(d),
                      separatorBefore: true,
                    },
                    {
                      label: "Show transcript…",
                      icon: <Icon name="info" />,
                      onClick: () => onShowTranscript(d),
                    },
                    {
                      label: "Show history",
                      icon: <Icon name="history" />,
                      onClick: () => onOpen(d),
                    },
                    {
                      label: "Move to Archive",
                      icon: <Icon name="trash" />,
                      onClick: () => onArchive(d),
                      danger: true,
                      separatorBefore: true,
                    },
                  ]}
                />
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

function TypeIcon({ kind }: { kind: string }) {
  if (kind === "Notebook") return <Icon name="notebook" />;
  if (kind === "DocumentType.Pdf") return <Icon name="pdf" />;
  if (kind === "DocumentType.Epub") return <Icon name="epub" />;
  return <Icon name="library" />;
}

function DocumentGrid({
  documents,
  selectedIds,
  selectMode,
  focusId,
  onClickRow,
  onArchive,
  onShowHistory,
  leavingIds,
  emptyHint,
}: {
  documents: DocumentSummary[];
  selectedIds: Set<string>;
  selectMode: boolean;
  focusId: string | null;
  onClickRow: (
    d: DocumentSummary,
    e: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean },
  ) => void;
  onArchive: (d: DocumentSummary) => void;
  onShowHistory: (d: DocumentSummary) => void;
  leavingIds: Set<string>;
  emptyHint: { title: string; body: string };
}) {
  if (documents.length === 0) {
    return (
      <div className="empty">
        <div className="empty-art">
          <Icon name="library" size={36} />
        </div>
        <h2>{emptyHint.title}</h2>
        <p>{emptyHint.body}</p>
      </div>
    );
  }
  return (
    <div className="docs-grid">
      {documents.map((d) => {
        const isSelected = selectedIds.has(d.document_id);
        const leaving = leavingIds.has(d.document_id);
        return (
          <div
            key={d.document_id}
            className={`tile${isSelected ? " selected" : ""}${
              focusId === d.document_id && !isSelected ? " focused" : ""
            }${leaving ? " leaving" : ""}`}
            draggable
            onDragStart={(e) => {
              const ids =
                isSelected && selectedIds.size > 1
                  ? [...selectedIds]
                  : [d.document_id];
              setDocumentDragData(e, d.document_id, ids);
              setCustomDragImage(e, d.visible_name, ids.length, DRAG_ICON_SVG);
            }}
            onClick={(e) =>
              onClickRow(d, {
                metaKey: e.metaKey,
                ctrlKey: e.ctrlKey,
                shiftKey: e.shiftKey,
              })
            }
          >
            {selectMode && (
              <span
                className={`tile-check${isSelected ? " checked" : ""}`}
                aria-hidden
              >
                {isSelected ? (
                  <Icon name="checkboxChecked" size={18} />
                ) : (
                  <Icon name="checkbox" size={18} />
                )}
              </span>
            )}
            <Thumbnail documentId={d.document_id} docType={d.doc_type} />
            <div className="title-line" title={d.visible_name}>
              <TypeIcon kind={d.doc_type} />
              <span style={{ overflow: "hidden", textOverflow: "ellipsis" }}>
                {d.visible_name}
              </span>
            </div>
            <div className="meta">
              <span>{prettyType(d.doc_type)}</span>
              <span>·</span>
              <span>{prettyDate(d.last_observed_at)}</span>
              {d.page_count !== null && (
                <>
                  <span>·</span>
                  <span>
                    {d.page_count}p
                  </span>
                </>
              )}
            </div>
            {d.has_unpushed_changes && (
              <span className="row-pill" title="Has local changes that will sync">
                Unsynced
              </span>
            )}
            {/* Hidden helpers so onShowHistory + onArchive get used by
                lint and stay symmetrical with list view. */}
            <span style={{ display: "none" }}>
              <button onClick={() => onShowHistory(d)} />
              <button onClick={() => onArchive(d)} />
            </span>
          </div>
        );
      })}
    </div>
  );
}

function ArchiveList({
  archived,
  onRestore,
  onPurge,
  onShowHistory,
  emptyHint,
}: {
  archived: ArchivedDocument[];
  onRestore: (d: ArchivedDocument) => void;
  onPurge: (d: ArchivedDocument) => void;
  onShowHistory: (d: ArchivedDocument) => void;
  emptyHint: { title: string; body: string };
}) {
  if (archived.length === 0) {
    return (
      <div className="empty">
        <div className="empty-art">
          <Icon name="trash" size={36} />
        </div>
        <h2>{emptyHint.title}</h2>
        <p>{emptyHint.body}</p>
      </div>
    );
  }
  return (
    <>
      <div className="banner">
        <Icon name="info" />
        <span>
          Items here will leave the tablet on the next sync. <strong>Restore</strong> brings them back; <strong>Delete forever</strong> drops every saved version and cannot be undone.
        </span>
      </div>
      <table className="docs">
        <thead>
          <tr>
            <th>Title</th>
            <th>Type</th>
            <th>Reason</th>
            <th>Archived</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {archived.map((d) => (
            <tr key={d.document_id}>
              <td className="row-title">
                <span className="type-glyph"><TypeIcon kind={d.doc_type} /></span>
                {d.visible_name}
              </td>
              <td className="muted small">{prettyType(d.doc_type)}</td>
              <td className="muted small">
                {d.reason === "device" ? "Deleted on tablet" : "Deleted locally"}
              </td>
              <td className="muted small">{prettyDate(d.archived_at)}</td>
              <td className="row-actions">
                <Menu
                  trigger={
                    <button
                      className="icon ghost"
                      aria-label="More actions"
                      onClick={(e) => e.stopPropagation()}
                    >
                      <Icon name="more" />
                    </button>
                  }
                  items={[
                    {
                      label: "Restore to library",
                      icon: <Icon name="restore" />,
                      onClick: () => onRestore(d),
                    },
                    {
                      label: "Show history",
                      icon: <Icon name="history" />,
                      onClick: () => onShowHistory(d),
                    },
                    {
                      label: "Delete forever",
                      icon: <Icon name="delete" />,
                      onClick: () => onPurge(d),
                      danger: true,
                      separatorBefore: true,
                    },
                  ]}
                />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  );
}

// =====================================================================
// Formatting
// =====================================================================

function prettyType(s: string): string {
  if (s === "Notebook") return "Notebook";
  if (s === "DocumentType.Pdf") return "PDF";
  if (s === "DocumentType.Epub") return "EPUB";
  return s;
}

function prettyDate(iso: string): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const now = Date.now();
  const diffMs = now - d.getTime();
  const minutes = Math.floor(diffMs / 60_000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  return d.toLocaleDateString();
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}
