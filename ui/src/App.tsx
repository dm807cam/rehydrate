import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type DragEvent as ReactDragEvent,
  type ReactNode,
} from "react";
import {
  ipc,
  MAX_IMPORT_FILE_BYTES,
  onHostKeyWarning,
  onKeyringWarning,
  onLegacyFormatWarning,
} from "./ipc";
import { HistoryDrawer } from "./components/HistoryDrawer";
import { LogDrawer } from "./components/LogDrawer";
import { PasswordDialog } from "./components/PasswordDialog";
import { StatusPill } from "./components/StatusPill";
import { SyncDrawer } from "./components/SyncDrawer";
import { Icon } from "./components/Icon";
import { LibrarySwitcher } from "./components/LibrarySwitcher";
import { Menu } from "./components/Menu";
import { Skeleton } from "./components/Skeleton";
import { useToast } from "./components/Toast";
import { useConfirm } from "./components/Confirm";
import { AboutDialog } from "./components/AboutDialog";
import { Cheatsheet } from "./components/Cheatsheet";
import { SettingsModal } from "./components/SettingsModal";
import { TranscriptDrawer } from "./components/TranscriptDrawer";
import { OcrJobChip } from "./components/OcrJobChip";
import { CommandPalette, type PaletteItem } from "./components/CommandPalette";
import { Onboarding } from "./components/Onboarding";
import { QuickLook } from "./components/QuickLook";
import { RenameDialog } from "./components/RenameDialog";
import { NamePrompt } from "./components/NamePrompt";
import { ChooseFolderDialog } from "./components/ChooseFolderDialog";
import { Thumbnail, invalidateThumbnail } from "./components/Thumbnail";
import { DRAG_ICON_SVG, setCustomDragImage } from "./dragImage";
import {
  activeFolderDragIdSnapshot,
  clearActiveFolderDrag,
  computeReorderSortIndex,
  descendantIds,
  hasDocumentDragData,
  hasFolderDragData,
  readDocumentDragData,
  readFolderDragData,
  setDocumentDragData,
  setFolderDragData,
  startNativeExportDrag,
} from "./drag";
import { formatError } from "./formatError";
import type {
  ArchivedDocument,
  DocumentSummary,
  FolderEntry,
} from "./types";
import { useDeviceSync } from "./hooks/useDeviceSync";
import { useLibrary } from "./hooks/useLibrary";
import { useOcr } from "./hooks/useOcr";
import { useSelection } from "./hooks/useSelection";
import {
  countByKind,
  emptyHintFor,
  filterDocuments,
  loadPersistedView,
  loadPersistedViewMode,
  persistView,
  persistViewMode,
  recentCount,
  summaryHealth,
  unsyncedCount,
  type View,
  viewKey,
  viewSubtitle,
  viewTitle,
} from "./views";

export function App() {
  const [defaultPath, setDefaultPath] = useState<string | null>(null);
  // Library content state + refresh callbacks live in `useLibrary`
  // — see `./hooks/useLibrary.ts`. The hook owns the four content
  // lists, the recents list, the open-state pair, and the canonical
  // refresh path. Imperative orchestrators (openLibrary,
  // switchToLibrary, openAnotherLibrary) stay in App.tsx because
  // they cross selection / expanded / confirm boundaries.
  const {
    libraryOpen,
    libraryPath,
    recentLibraries,
    summary,
    documents,
    folders,
    archived,
    setLibraryOpen,
    setLibraryPath,
    setSummary,
    setDocuments,
    setFolders,
    setArchived,
    refreshLibrary,
    refreshRecentLibraries,
    clearLibraryUi,
  } = useLibrary({ setError: (msg) => setError(msg) });
  // Device connection state + tryConnect/disconnect/submitPassword
  // live in `useDeviceSync` — see `./hooks/useDeviceSync.ts`. The
  // hook owns the `device:reachable` Tauri event listener and the
  // three connect-flow actions.
  const { device, setDevice, tryConnect, disconnect, submitPassword } =
    useDeviceSync({
      setError: (msg) => setError(msg),
      openPasswordDialog: () => setShowPassword(true),
      closePasswordDialog: () => setShowPassword(false),
    });
  const [error, setError] = useState<string | null>(null);
  const [showPassword, setShowPassword] = useState(false);
  const [showSync, setShowSync] = useState(false);
  const [historyDoc, setHistoryDoc] = useState<DocumentSummary | null>(null);
  const [showLogs, setShowLogs] = useState(false);
  const [view, setView] = useState<View>(() => loadPersistedView());
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  // Selection + keyboard-focus state lives in `useSelection` —
  // see `./hooks/useSelection.ts` for the rationale and the
  // selection/focus duality.
  // `anchorId` isn't read here — the hook's internal `handleRowClick`
  // is the only consumer — but `setAnchorId` is used by the
  // keyboard cascade in the body.
  const {
    selectedIds,
    focusId,
    selectMode,
    selectedId,
    setSelectedIds,
    setAnchorId,
    setFocusId,
    setSelectMode,
    handleRowClick,
    toggleSelectMode,
    clearSelection,
    clearFocus,
  } = useSelection(openInViewer);
  const [syncPhase, setSyncPhase] = useState<"idle" | "syncing" | "failed">("idle");
  const [search, setSearch] = useState("");
  const [showSearch, setShowSearch] = useState(false);
  const [leavingIds, setLeavingIds] = useState<Set<string>>(new Set());
  const [viewMode, setViewMode] = useState<"list" | "grid">(() =>
    loadPersistedViewMode(),
  );
  const [quickLookId, setQuickLookId] = useState<string | null>(null);
  const [showCheatsheet, setShowCheatsheet] = useState(false);
  const [showAbout, setShowAbout] = useState(false);
  const [showPalette, setShowPalette] = useState(false);
  const [showOnboarding, setShowOnboarding] = useState(false);
  const [renaming, setRenaming] = useState<
    | { kind: "document"; id: string; current: string }
    | { kind: "folder"; id: string; current: string }
    | null
  >(null);
  // "+ New folder" / "New subfolder" prompt state. `parentId = null`
  // means root; otherwise a folder id.
  const [creatingFolderUnder, setCreatingFolderUnder] = useState<
    { parentId: string | null } | null
  >(null);
  // "Move to folder…" picker state. Holds the doc ids the user wants
  // to relocate; null means the dialog is closed.
  const [movingDocs, setMovingDocs] = useState<string[] | null>(null);
  // True while the user is dragging files from outside the app
  // (Finder, Explorer, …) over the window. Drives the import overlay.
  const [externalDrop, setExternalDrop] = useState(false);
  // True while a drop is being processed so the overlay can show
  // "Importing…" feedback instead of dismissing instantly.
  const [importingDrop, setImportingDrop] = useState(false);
  // Settings modal state. `null` = closed; otherwise picks which tab
  // to mount with and an optional banner shown above the form (used
  // when the modal is auto-opened to explain why).
  const [settings, setSettings] = useState<
    | { tab: "ollama" | "publishing"; banner: string | null }
    | null
  >(null);
  // The document the user wants to view a transcript for. Mounted
  // as a side-drawer; backed by `ipc.getTranscript`.
  const [transcriptDoc, setTranscriptDoc] = useState<DocumentSummary | null>(
    null,
  );
  // OCR + auto-OCR sweep state, listener, and the start/dismiss
  // actions live in the `useOcr` hook (see `./hooks/useOcr.ts`).
  // It's instantiated below, after `refreshLibrary` is defined.
  // Idempotency guard for the per-library-open sweep effect: stores
  // the library path that's already been kicked off so a re-render
  // doesn't re-fire the sweep for the same library.
  const autoOcrInitialised = useRef<string | null>(null);
  const searchInputRef = useRef<HTMLInputElement | null>(null);
  const toast = useToast();
  const confirm = useConfirm();

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
        if (!cancelled) setError(formatError(e));
      }
    })();
    return () => {
      cancelled = true;
    };
    // The setters and refresh callbacks are stable references from
    // their respective hooks; this effect should run exactly once,
    // at mount. Listing them would inflate the dep array without
    // changing behaviour.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // The `device:reachable` listener lives in `useDeviceSync`; see
  // the hook for the why.

  // ---- Keyring warnings ------------------------------------------------
  // Emitted when we couldn't persist the device password (most often
  // a Linux box without secret-service running). Surface as a toast
  // so the user knows future connects will re-ask for the password.
  useEffect(() => {
    // Issue #37: a fast unmount-before-resolve would leave the
    // listener permanently attached and its closure holding setState
    // refs to an unmounted component. Track `cancelled` so the
    // promise resolver either installs the unlisten or invokes it
    // immediately if cleanup has already run.
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    onKeyringWarning((msg) => {
      toast.show({ tone: "warn", body: msg, duration: 9000 });
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [toast]);

  // ---- Host-key warnings -----------------------------------------------
  // The TOFU layer accepted a first-seen host key but couldn't write
  // it to the on-disk known-hosts store. Connection works; the next
  // reconnect just won't have a fingerprint to compare against until
  // the underlying FS/permissions issue is fixed.
  useEffect(() => {
    // Issue #37: see the keyring-warning effect above for the
    // unmount-before-resolve race this `cancelled` flag closes.
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    onHostKeyWarning((msg) => {
      toast.show({ tone: "warn", body: msg, duration: 12000 });
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [toast]);

  // ---- Legacy notebook format warning ----------------------------------
  // Emitted when an older v3/v5 .rm file falls back to the thumbnail
  // preview path. The viewer still opens; we just want the user to
  // understand why the result looks fuzzy.
  useEffect(() => {
    // Issue #37: see the keyring-warning effect above for the
    // unmount-before-resolve race this `cancelled` flag closes.
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    onLegacyFormatWarning((msg) => {
      toast.show({ tone: "info", body: msg, duration: 7000 });
    }).then((u) => {
      if (cancelled) u();
      else unlisten = u;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [toast]);

  // `refreshLibrary` and `refreshRecentLibraries` are provided by
  // `useLibrary` (destructured at the top of this component).

  // ---- Library / device commands --------------------------------------

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
      setError(formatError(e));
    }
  }

  // Switch to a library that's already in the recents list (shown in
  // the library-switcher dropdown).
  async function switchToLibrary(path: string) {
    if (path === libraryPath) return;
    setError(null);
    // Clear stale UI before fetching the new library so the user sees
    // an obvious "loading" state instead of cross-library leakage.
    clearLibraryUi();
    setSelectedIds(new Set());
    setFocusId(null);
    setExpanded(new Set());
    try {
      const opened = await ipc.switchLibrary(path);
      setLibraryPath(opened);
      await Promise.all([refreshLibrary(), refreshRecentLibraries()]);
    } catch (e) {
      setError(formatError(e));
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
      clearLibraryUi();
      setSelectedIds(new Set());
      setFocusId(null);
      setExpanded(new Set());

      await ipc.openLibrary(picked.path);
      setLibraryOpen(true);
      setLibraryPath(picked.path);
      setShowOnboarding(false);
      await Promise.all([refreshLibrary(), refreshRecentLibraries()]);
    } catch (e) {
      setError(formatError(e));
    }
  }

  // Row click + selection lives in `useSelection`. `handleRowClick`,
  // `toggleSelectMode`, `clearSelection`, `clearFocus` are
  // destructured into scope at the top of this component.

  // While Quick Look is open, follow the keyboard cursor — pressing
  // ↑/↓ updates the previewed document, mirroring macOS Finder /
  // Apple Mail. Only re-aim while QL is already showing; pressing
  // Space from the list still picks up the current selection at
  // open time (handled in the keyboard cascade).
  useEffect(() => {
    if (!quickLookId || !selectedId) return;
    if (selectedId !== quickLookId) setQuickLookId(selectedId);
  }, [quickLookId, selectedId]);

  // Persist the last view + viewMode across launches. Power users
  // who live in "Pending Sync" or grid view shouldn't have to set
  // them every cold start. Folder views aren't persisted because
  // folder ids may not exist after a library switch.
  useEffect(() => {
    persistView(view);
  }, [view]);
  useEffect(() => {
    persistViewMode(viewMode);
  }, [viewMode]);

  // `tryConnect` / `disconnect` / `submitPassword` are provided by
  // `useDeviceSync` (destructured at the top of this component).

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
    if (!libraryOpen) {
      toast.show({ tone: "warn", body: "Open a library first." });
      return;
    }
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
      setError(formatError(e));
    }
  }

  async function garbageCollect() {
    if (!libraryOpen) {
      toast.show({ tone: "warn", body: "Open a library first." });
      return;
    }
    // Spell out exactly what "clean up unused files" means before
    // running it — novices reasonably expect a maintenance action to
    // ask first. The op is safe (only deletes blobs no manifest
    // references) but a confirm avoids surprise.
    const ok = await confirm({
      title: "Clean up unused files?",
      body: (
        <>
          <p>
            Scans your library for stored files that no version of any
            document references — usually leftovers from purging an
            archived document or restoring an older version. Safe to run
            any time.
          </p>
        </>
      ),
      confirmLabel: "Clean up",
    });
    if (!ok) return;
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
      setError(formatError(e));
    }
  }

  async function verify() {
    if (!libraryOpen) {
      toast.show({ tone: "warn", body: "Open a library first." });
      return;
    }
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
      setError(formatError(e));
    }
  }

  async function openInViewer(d: DocumentSummary) {
    setError(null);
    try {
      await ipc.openDocument(d.document_id);
    } catch (e) {
      setError(formatError(e));
    }
  }

  // ⌥-drag handler: ask the backend to stage a clean-filename copy
  // (rendering the notebook to PDF if needed) and hand the path to
  // the OS via `tauri-plugin-drag`. Wrapped in useCallback so the
  // child rows / tiles don't get a fresh function identity every
  // render — they'd otherwise drop their drag-start memoisation.
  // Errors surface as a toast: macOS' drag gesture has a short
  // window and a cold cache can blow past it; the toast tells the
  // user to retry rather than leave them wondering why nothing
  // happened.
  const startExportDrag = useCallback(
    async (d: DocumentSummary) => {
      try {
        const paths = await ipc.prepareExportPdf(d.document_id);
        await startNativeExportDrag(paths);
      } catch (e) {
        toast.show({
          tone: "warn",
          body: `Couldn't start drag for "${d.visible_name}": ${formatError(e)}`,
          duration: 6000,
        });
      }
    },
    [toast],
  );

  // Hover prefetch: warms the export cache so that when the user
  // actually starts the ⌥-drag, the IPC returns instantly and
  // `startDrag` fires inside macOS' user-gesture window. Cheap on
  // cache hit (one stat); on cache miss it pre-renders the PDF in
  // the background. Errors are swallowed — this is a best-effort
  // optimisation and the dragstart path will report any real
  // failure.
  //
  // Dedup on the JS side so a fast scroll across 100 tiles doesn't
  // queue 100 cold renders. The backend command is already
  // idempotent (it checks for an existing staging file before
  // rendering), but skipping the IPC round-trip entirely is still
  // cheaper.
  const prefetchedExportsRef = useRef<Set<string>>(new Set());
  const prefetchExportDrag = useCallback((documentId: string) => {
    if (prefetchedExportsRef.current.has(documentId)) return;
    prefetchedExportsRef.current.add(documentId);
    void ipc.prepareExportPdf(documentId).catch(() => {
      // The dragstart path will retry — clear the dedup so the
      // user's actual gesture isn't short-circuited by a stale
      // failure (e.g. a sync that finished and changed the
      // manifest hash between the prefetch and the drag).
      prefetchedExportsRef.current.delete(documentId);
    });
  }, []);

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
                  toast.show({ tone: "err", body: formatError(e) });
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
                  toast.show({ tone: "err", body: formatError(e) });
                }
              },
            },
          });
        }
      } catch (e) {
        setError(formatError(e));
      }
    },
    // `bulkArchive` and `bulkMove` are declared further down in this
    // module and are captured via closure; including them here would
    // force a re-render every keystroke without changing behaviour.
    // eslint-disable-next-line react-hooks/exhaustive-deps
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

  // ---- Folder create / move-document helpers ---------------------------
  function startCreateFolder(parentId: string | null) {
    setCreatingFolderUnder({ parentId });
  }
  async function performCreateFolder(name: string) {
    if (!creatingFolderUnder) return;
    const parent = creatingFolderUnder.parentId;
    const created = await ipc.createFolder(name, parent);
    // Auto-expand the new folder's parent so the result is visible.
    if (parent) {
      setExpanded((prev) => {
        if (prev.has(parent)) return prev;
        const next = new Set(prev);
        next.add(parent);
        return next;
      });
    }
    await refreshLibrary();
    setCreatingFolderUnder(null);
    toast.show({
      tone: "ok",
      body: (
        <span>
          Created <strong>{created.visible_name}</strong>. The folder uploads to
          the tablet on the next sync.
        </span>
      ),
    });
  }

  async function startDeleteFolder(f: FolderEntry) {
    // Word the confirm body around where the children land so the
    // user isn't surprised by what happens to BH and test inside
    // "Journal" — the historical worry that drove this feature.
    const parent =
      f.parent && folders?.find((x) => x.folder_id === f.parent);
    const destination = parent ? (
      <>
        the parent folder <strong>{parent.visible_name}</strong>
      </>
    ) : (
      <>the top level</>
    );
    const ok = await confirm({
      title: `Delete folder "${f.visible_name}"?`,
      body: (
        <>
          Notebooks and subfolders inside it move to {destination}.
          Nothing is deleted from your library. The folder is also
          removed from the tablet on the next sync.
        </>
      ),
      confirmLabel: "Delete folder",
      destructive: true,
    });
    if (!ok) return;
    setError(null);
    try {
      const outcome = await ipc.deleteFolder(f.folder_id);
      // If the user happens to be looking at the deleted folder,
      // bounce them out of the dead view so they don't see an
      // empty "this folder no longer exists" screen.
      if (
        typeof view === "object" &&
        view.kind === "folder" &&
        view.id === f.folder_id
      ) {
        setView("all");
      }
      await refreshLibrary();
      const moved =
        outcome.folders_moved + outcome.documents_moved;
      toast.show({
        tone: "ok",
        body:
          moved === 0 ? (
            <>
              Deleted folder <strong>{f.visible_name}</strong>. It is
              removed from the tablet on the next sync.
            </>
          ) : (
            <>
              Deleted folder <strong>{f.visible_name}</strong>.{" "}
              {outcome.documents_moved > 0 && (
                <>
                  {outcome.documents_moved} document
                  {outcome.documents_moved === 1 ? "" : "s"}
                </>
              )}
              {outcome.documents_moved > 0 && outcome.folders_moved > 0
                ? " and "
                : ""}
              {outcome.folders_moved > 0 && (
                <>
                  {outcome.folders_moved} subfolder
                  {outcome.folders_moved === 1 ? "" : "s"}
                </>
              )}{" "}
              moved up.
            </>
          ),
      });
    } catch (e) {
      setError(formatError(e));
    }
  }

  // ---- Revert unpushed changes ----------------------------------------
  //
  // User-facing escape hatch for "I made a local edit I now regret
  // and I don't want it to sync." Folder deletions in particular
  // are otherwise unrecoverable — the original bug report ran "if
  // I for example delete a folder I am trapped and will have to
  // sync it." Revert rolls every unpushed folder edit AND every
  // unpushed document move/rename back to the last-synced state.
  // Locally-imported documents are intentionally NOT touched — the
  // backend (`Library::revert_unpushed_changes`) leaves them
  // because a Revert button that silently deleted a fresh PDF
  // import would be a worse surprise than any of the edits it's
  // trying to undo.
  async function revertChanges() {
    const ok = await confirm({
      title: "Revert all changes since the last sync?",
      body: (
        <>
          Folder renames, moves, deletions, and new folders go back to
          their last-synced state. Document moves and renames you
          haven't synced yet are also undone. Imported PDFs and EPUBs
          stay put — use Archive to remove those.
        </>
      ),
      confirmLabel: "Revert",
      destructive: true,
    });
    if (!ok) return;
    setError(null);
    try {
      const report = await ipc.revertUnpushedChanges();
      await refreshLibrary();
      const total =
        report.folders_restored +
        report.folders_dropped +
        report.documents_rolled_back;
      if (total === 0) {
        toast.show({
          tone: "info",
          body: "Nothing to revert — the library is already in sync.",
        });
        return;
      }
      // Build a precise summary so the user can confirm exactly
      // what was undone. Pieces only appear when their count is
      // non-zero, which keeps the toast tight on the common
      // single-action revert.
      const pieces: string[] = [];
      if (report.folders_restored > 0) {
        pieces.push(
          `${report.folders_restored} folder${report.folders_restored === 1 ? "" : "s"} restored`,
        );
      }
      if (report.folders_dropped > 0) {
        pieces.push(
          `${report.folders_dropped} new folder${report.folders_dropped === 1 ? "" : "s"} removed`,
        );
      }
      if (report.documents_rolled_back > 0) {
        pieces.push(
          `${report.documents_rolled_back} document${report.documents_rolled_back === 1 ? "" : "s"} rolled back`,
        );
      }
      toast.show({ tone: "ok", body: `Reverted: ${pieces.join(", ")}.` });
    } catch (e) {
      setError(formatError(e));
    }
  }

  function startMoveDocs(ids: string[]) {
    if (ids.length === 0) return;
    setMovingDocs(ids);
  }
  async function performMoveDocs(target: string | null) {
    if (!movingDocs) return;
    const ids = movingDocs;
    setError(null);
    try {
      for (const id of ids) {
        await ipc.moveDocument(id, target);
      }
      await refreshLibrary();
      clearSelection();
      const targetName =
        target === null
          ? "the library root"
          : folders.find((f) => f.folder_id === target)?.visible_name ??
            "another folder";
      toast.show({
        tone: "info",
        body: `Moved ${ids.length} document${ids.length === 1 ? "" : "s"} to ${targetName}.`,
      });
    } catch (e) {
      setError(formatError(e));
    } finally {
      setMovingDocs(null);
    }
  }

  // ---- External file drop import (PDF / EPUB from Finder) -------------
  // dragenter/leave on nested children fire repeatedly; tracking a
  // counter on a ref keeps the overlay stable until the cursor truly
  // leaves the window.
  const externalDropCounter = useRef(0);
  const isExternalFileDrag = (e: ReactDragEvent) =>
    Array.from(e.dataTransfer.types).includes("Files");

  async function handleExternalFileDrop(files: File[]) {
    if (!libraryOpen) {
      toast.show({
        tone: "warn",
        body: "Open a library first, then drop PDFs or EPUBs here to import.",
      });
      return;
    }
    const importable = files.filter((f) => {
      const lower = f.name.toLowerCase();
      return lower.endsWith(".pdf") || lower.endsWith(".epub");
    });
    if (importable.length === 0) {
      toast.show({
        tone: "warn",
        body: "Only PDF and EPUB files can be imported.",
      });
      return;
    }
    setImportingDrop(true);
    let imported = 0;
    let failed = 0;
    for (const file of importable) {
      // Preflight on `file.size` BEFORE `arrayBuffer()`. Without this
      // guard a 500 MB drop would allocate the full Uint8Array in the
      // renderer plus ~4× that in the JSON-IPC encoder before the
      // backend's MAX_IMPORT_FILE_BYTES check fired — long enough to
      // freeze or OOM the UI (issue #24). `File.size` is metadata, no
      // bytes are read.
      if (file.size > MAX_IMPORT_FILE_BYTES) {
        failed += 1;
        toast.show({
          tone: "err",
          body: `${file.name} is too large to import (${formatBytes(
            file.size,
          )}; limit is ${formatBytes(MAX_IMPORT_FILE_BYTES)}).`,
        });
        continue;
      }
      try {
        const buf = await file.arrayBuffer();
        await ipc.importDroppedFile(file.name, new Uint8Array(buf));
        imported += 1;
      } catch (e) {
        failed += 1;
        // Report each failure but keep going so a single bad file
        // doesn't abort the whole drop.
        toast.show({
          tone: "err",
          body: `Couldn't import ${file.name}: ${formatError(e)}`,
        });
      }
    }
    setImportingDrop(false);
    if (imported > 0) {
      await refreshLibrary();
      toast.show({
        tone: "ok",
        body:
          imported === 1
            ? "Imported 1 file. It will sync to the tablet on the next sync."
            : `Imported ${imported} files. They will sync on the next sync.`,
      });
    }
    if (failed > 0 && imported === 0) {
      setError(`Could not import ${failed} file${failed === 1 ? "" : "s"}.`);
    }
  }

  // ---- Folder reorder (sidebar drag-and-drop) --------------------------
  // Computes a sort_index that places `dragged` adjacent to `target`
  // under `newParent`. `position = "above" | "below" | "into"` mirrors
  // the drop indicator the user saw. For "into" we slot at the end of
  // the target's children.
  const reorderFolderTo = useCallback(
    async (
      draggedId: string,
      newParent: string | null,
      newSortIndex: number,
    ) => {
      try {
        await ipc.reorderFolder(draggedId, newParent, newSortIndex);
        await refreshLibrary();
      } catch (e) {
        setError(formatError(e));
      }
    },
    [refreshLibrary],
  );

  // ---- OCR + auto-OCR sweep -------------------------------------------
  // The OCR sub-system (chip state, listener, elapsed timer, single-job
  // start, sweep start, dismiss) lives in `useOcr` — see
  // `./hooks/useOcr.ts` for the rationale and the token-based
  // cancellation contract.
  const {
    ocrJob,
    ocrJobElapsed,
    autoOcrSweep,
    startOcrJob,
    startAutoOcrSweep,
    dismissOcrChip,
  } = useOcr({
    refreshLibrary,
    toast,
    openSettings: (tab, banner) =>
      setSettings({ tab, banner: banner ?? null }),
    openTranscript: setTranscriptDoc,
  });

  // Fire the sweep when the library is open. `libraryPath` as a dep
  // gives us one sweep per library-open transition; the token
  // mechanism inside `startAutoOcrSweep` cleanly invalidates a
  // prior sweep that's still running for the previous library.
  useEffect(() => {
    if (!libraryOpen) return;
    if (autoOcrInitialised.current === libraryPath) return;
    autoOcrInitialised.current = libraryPath;
    void startAutoOcrSweep();
  }, [libraryOpen, libraryPath, startAutoOcrSweep]);

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
      setError(formatError(e));
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
      setError(formatError(e));
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
      setError(formatError(e));
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
      setError(formatError(e));
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
      {
        id: "act-about",
        label: "About reHydrate…",
        icon: <Icon name="info" />,
        tag: "Help",
        onRun: () => setShowAbout(true),
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
        if (showAbout) return setShowAbout(false);
        if (quickLookId) return setQuickLookId(null);
        if (showPassword) return setShowPassword(false);
        if (showLogs) return setShowLogs(false);
        if (showSync) return setShowSync(false);
        if (historyDoc) return setHistoryDoc(null);
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
        // Route through the same handler the toolbar button uses so
        // the user gets the "Connect the tablet first" / "Open a
        // library first" toasts instead of a silent no-op.
        openSync();
        return;
      }
      if (meta && (e.key === "i" || e.key === "I")) {
        e.preventDefault();
        importFile();
        return;
      }
      if (meta && (e.key === "Backspace" || e.key === "Delete")) {
        e.preventDefault();
        if (selectedIds.size > 0 && documents) bulkArchive();
        return;
      }
      if (e.key === " ") {
        // Space → Quick Look the focused row. If QL is already open,
        // its own keydown handler closes it; suppress the App-level
        // re-open path so Space works as a toggle.
        if (quickLookId) return;
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
        // QuickLook handles Enter locally (open + dismiss). Don't
        // double-fire the open call from here.
        if (quickLookId) return;
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
    showAbout,
    showPalette,
    quickLookId,
    showPassword,
    renaming,
    selectMode,
    focusId,
    libraryOpen,
    device?.connected,
    documents,
    filteredDocs,
  ]);

  // ---- Render -----------------------------------------------------------
  return (
    <div
      className="app"
      onDragEnter={(e) => {
        if (!isExternalFileDrag(e)) return;
        e.preventDefault();
        externalDropCounter.current += 1;
        if (!externalDrop) setExternalDrop(true);
      }}
      onDragOver={(e) => {
        if (!isExternalFileDrag(e)) return;
        // Without preventDefault the browser refuses the drop and
        // shows the "no-entry" cursor.
        e.preventDefault();
        e.dataTransfer.dropEffect = "copy";
      }}
      onDragLeave={(e) => {
        if (!isExternalFileDrag(e)) return;
        externalDropCounter.current = Math.max(
          0,
          externalDropCounter.current - 1,
        );
        if (externalDropCounter.current === 0) setExternalDrop(false);
      }}
      onDrop={(e) => {
        if (!isExternalFileDrag(e)) return;
        e.preventDefault();
        externalDropCounter.current = 0;
        setExternalDrop(false);
        const files = Array.from(e.dataTransfer.files);
        if (files.length > 0) void handleExternalFileDrop(files);
      }}
    >
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
              onClick={revertChanges}
              title="Revert local folder + document changes since the last sync"
            >
              <Icon name="restore" /> Revert
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
              label: "Settings…",
              icon: <Icon name="settings" />,
              onClick: () => setSettings({ tab: "ollama", banner: null }),
              separatorBefore: true,
            },
            {
              label: "Activity log",
              icon: <Icon name="info" />,
              onClick: () => setShowLogs(true),
            },
            {
              label: "Keyboard shortcuts",
              icon: <Icon name="info" />,
              onClick: () => setShowCheatsheet(true),
            },
            {
              label: "About reHydrate…",
              icon: <Icon name="info" />,
              onClick: () => setShowAbout(true),
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

          {libraryOpen && (
            <section>
              <div className="sidebar-section-head">
                <h3>Folders</h3>
                <button
                  className="icon ghost sidebar-add"
                  aria-label="New folder"
                  title="New folder at the root"
                  onClick={() => startCreateFolder(null)}
                >
                  +
                </button>
              </div>
              {folders.length > 0 && documents ? (
                <FolderTree
                  folders={folders}
                  documents={documents}
                  view={view}
                  setView={setView}
                  expanded={expanded}
                  setExpanded={setExpanded}
                  onDocumentDrop={handleDocumentDrop}
                  onRenameFolder={startRenameFolder}
                  onCreateSubfolder={(parentId) => startCreateFolder(parentId)}
                  onDeleteFolder={startDeleteFolder}
                  onReorderFolder={(draggedId, newParent, beforeId, afterId) => {
                    const newSort = computeReorderSortIndex(
                      folders,
                      draggedId,
                      newParent,
                      beforeId,
                      afterId,
                    );
                    return reorderFolderTo(draggedId, newParent, newSort);
                  }}
                />
              ) : (
                <p className="sidebar-empty muted">
                  No folders yet — click <strong>+</strong> to create one.
                </p>
              )}
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
            <div className="error" role="alert">
              <Icon name="warn" />
              <span>{error}</span>
              <button
                onClick={() => setError(null)}
                className="close-inline"
                aria-label="Dismiss error"
                title="Dismiss"
              >
                ×
              </button>
            </div>
          )}

          {!libraryOpen && showOnboarding ? (
            <Onboarding
              defaultPath={defaultPath}
              initialDevice={device}
              onOpenLibrary={openLibrary}
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
                      aria-label="Search this view"
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
                  <button onClick={() => startMoveDocs([...selectedIds])}>
                    <Icon name="folder" /> Move to folder…
                  </button>
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
                  onOpenInViewer={openInViewer}
                  onRename={startRenameDocument}
                  onArchive={archiveOne}
                  onShowHistory={setHistoryDoc}
                  onMove={(d) => startMoveDocs([d.document_id])}
                  onTranscribe={(d) => startOcrJob(d)}
                  onViewTranscript={setTranscriptDoc}
                  onStartExportDrag={startExportDrag}
                  onPrefetchExport={prefetchExportDrag}
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
                  onMove={(d) => startMoveDocs([d.document_id])}
                  onTranscribe={(d) => startOcrJob(d)}
                  onViewTranscript={setTranscriptDoc}
                  onStartExportDrag={startExportDrag}
                  onPrefetchExport={prefetchExportDrag}
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
            onSyncStateChange={setSyncPhase}
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
      {creatingFolderUnder && (
        <NamePrompt
          title={
            creatingFolderUnder.parentId === null
              ? "New folder"
              : "New subfolder"
          }
          subtitle={
            creatingFolderUnder.parentId === null
              ? "The new folder uploads to the tablet on the next sync."
              : `Inside ${
                  folders.find(
                    (f) => f.folder_id === creatingFolderUnder.parentId,
                  )?.visible_name ?? "selected folder"
                }. Uploads on the next sync.`
          }
          placeholder="Folder name"
          submitLabel="Create"
          submitBusyLabel="Creating…"
          onCancel={() => setCreatingFolderUnder(null)}
          onSubmit={performCreateFolder}
        />
      )}
      {movingDocs && (
        <ChooseFolderDialog
          title={
            movingDocs.length === 1
              ? "Move document"
              : `Move ${movingDocs.length} documents`
          }
          subtitle="Pick a destination. The change syncs to the tablet on the next sync."
          folders={folders}
          onCancel={() => setMovingDocs(null)}
          onChoose={performMoveDocs}
          onFolderCreated={() => {
            // Pull the freshly-created folder into the parent's
            // state so the picker shows it as a selectable target
            // for "Move here".
            void refreshLibrary();
          }}
        />
      )}
      {settings && (
        <SettingsModal
          initialTab={settings.tab}
          banner={settings.banner}
          onClose={() => setSettings(null)}
          notify={(tone, body) => toast.show({ tone, body })}
        />
      )}
      {transcriptDoc && (
        <TranscriptDrawer
          document={transcriptDoc}
          onClose={() => setTranscriptDoc(null)}
          notify={(tone, body) => toast.show({ tone, body })}
          onOpenSettings={(tab, banner) =>
            setSettings({ tab, banner: banner ?? null })
          }
        />
      )}
      {ocrJob && (
        <OcrJobChip
          job={ocrJob}
          elapsedSeconds={ocrJobElapsed}
          onDismiss={dismissOcrChip}
          sweep={autoOcrSweep}
        />
      )}
      {showCheatsheet && <Cheatsheet onClose={() => setShowCheatsheet(false)} />}
      {showAbout && <AboutDialog onClose={() => setShowAbout(false)} />}
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
      {(externalDrop || importingDrop) && (
        <div className="import-overlay" aria-hidden>
          <div className="import-overlay-card">
            <Icon name="library" size={36} />
            <h2>
              {importingDrop ? "Importing…" : "Drop to import"}
            </h2>
            <p>
              {importingDrop
                ? "Hashing and recording new versions in your library."
                : libraryOpen
                  ? "PDFs and EPUBs land in your library and sync to the tablet next sync."
                  : "Open a library first, then drop PDFs or EPUBs here."}
            </p>
          </div>
        </div>
      )}
    </div>
  );
}

// Drag-and-drop helpers + the per-view label/filter/persist helpers
// they reference live in dedicated files:
//   - `./drag`  — DataTransfer wire formats, descendant computation,
//                 reorder-sort-index midpoints.
//   - `./views` — `View` type, viewTitle/viewSubtitle, filterDocuments,
//                 localStorage round-trip.
// Both are imported at the top of this file.

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
  // Tint the drop highlight differently for Archive vs folders — a
  // doc landing in Archive is destructive (it leaves the library
  // listing), so we use the danger palette to make the difference
  // unmistakable while the user is mid-drag.
  const isArchiveTarget = dropTarget === "archive";
  const cls = [
    viewKey(view) === viewKey(v) ? "active" : "",
    dragOver ? "drop-target" : "",
    dragOver && isArchiveTarget ? "drop-target-danger" : "",
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
    nodes.sort((a, b) => {
      const cmp = (a.folder.sort_index ?? 0) - (b.folder.sort_index ?? 0);
      if (cmp !== 0) return cmp;
      return a.folder.visible_name.localeCompare(b.folder.visible_name);
    });
    for (const n of nodes) sortRec(n.children);
  };
  sortRec(roots);
  return roots;
}

type FolderReorder = (
  draggedId: string,
  newParent: string | null,
  beforeId: string | null,
  afterId: string | null,
) => Promise<void> | void;

function FolderTree({
  folders,
  documents,
  view,
  setView,
  expanded,
  setExpanded,
  onDocumentDrop,
  onRenameFolder,
  onCreateSubfolder,
  onDeleteFolder,
  onReorderFolder,
}: {
  folders: FolderEntry[];
  documents: DocumentSummary[];
  view: View;
  setView: (v: View) => void;
  expanded: Set<string>;
  setExpanded: (s: Set<string>) => void;
  onDocumentDrop: (documentId: string, target: string | null | "archive", batch?: string[]) => void;
  onRenameFolder: (f: FolderEntry) => void;
  onCreateSubfolder: (parentId: string) => void;
  onDeleteFolder: (f: FolderEntry) => void;
  onReorderFolder: FolderReorder;
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
      {roots.map((node, idx) => (
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
          onCreateSubfolder={onCreateSubfolder}
          onDeleteFolder={onDeleteFolder}
          onReorderFolder={onReorderFolder}
          allFolders={folders}
          siblings={roots.map((n) => n.folder.folder_id)}
          siblingIndex={idx}
          parentId={null}
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
  onCreateSubfolder,
  onDeleteFolder,
  onReorderFolder,
  allFolders,
  siblings,
  siblingIndex,
  parentId,
}: {
  node: FolderTreeNode;
  depth: number;
  view: View;
  setView: (v: View) => void;
  expanded: Set<string>;
  toggle: (id: string) => void;
  onDocumentDrop: (documentId: string, target: string | null | "archive", batch?: string[]) => void;
  onRenameFolder: (f: FolderEntry) => void;
  onCreateSubfolder: (parentId: string) => void;
  onDeleteFolder: (f: FolderEntry) => void;
  onReorderFolder: FolderReorder;
  allFolders: FolderEntry[];
  /** Ordered ids of this row's siblings under `parentId`, including
   *  this row. Used to pick neighbour ids for sort-index midpoints. */
  siblings: string[];
  siblingIndex: number;
  parentId: string | null;
}) {
  const v: View = { kind: "folder", id: node.folder.folder_id };
  const isActive = viewKey(view) === viewKey(v);
  const hasChildren = node.children.length > 0;
  const isOpen = expanded.has(node.folder.folder_id);
  const [dragOver, setDragOver] = useState(false);
  // For folder-on-folder drag we track which third of the row the
  // pointer is in: top → drop above, middle → drop into (reparent),
  // bottom → drop below. `null` while no compatible drag is hovering.
  const [folderDropZone, setFolderDropZone] = useState<
    "above" | "into" | "below" | null
  >(null);
  // Spring-loaded folder: hover during a drag for >600ms auto-expands.
  const hoverTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const cls = [
    "folder-row",
    isActive ? "active" : "",
    dragOver ? "drop-target" : "",
    folderDropZone ? `folder-drop-${folderDropZone}` : "",
  ]
    .filter(Boolean)
    .join(" ");

  const handleDragOver = (e: ReactDragEvent) => {
    if (hasFolderDragData(e)) {
      // `dataTransfer.getData(...)` is empty during dragover, so we
      // read the dragged id from the module-local stash. Forbid drop
      // when the dragged folder *is* this row (no-op move) or when
      // this row lives inside the dragged folder's subtree (would
      // create a cycle).
      const draggedId = activeFolderDragIdSnapshot();
      if (!draggedId) return;
      const subtree = descendantIds(allFolders, draggedId);
      if (subtree.has(node.folder.folder_id)) return;
      e.preventDefault();
      e.dataTransfer.dropEffect = "move";
      const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
      const y = e.clientY - rect.top;
      const h = rect.height;
      // 25% top → above, 25% bottom → below, middle → into.
      const zone: "above" | "into" | "below" =
        y < h * 0.25 ? "above" : y > h * 0.75 ? "below" : "into";
      // Auto-expand a non-empty target after hovering "into" for
      // 600ms so the user can drill deeper without manually toggling.
      if (zone === "into" && hasChildren && !isOpen && !hoverTimer.current) {
        hoverTimer.current = setTimeout(() => {
          toggle(node.folder.folder_id);
          hoverTimer.current = null;
        }, 600);
      }
      if (folderDropZone !== zone) setFolderDropZone(zone);
      return;
    }
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
  };
  const clearDropState = () => {
    setDragOver(false);
    setFolderDropZone(null);
    if (hoverTimer.current) {
      clearTimeout(hoverTimer.current);
      hoverTimer.current = null;
    }
  };

  return (
    <>
      <li
        className={cls}
        data-depth={depth}
        draggable
        onDragStart={(e) => {
          // Stop the parent <li> from also picking the drag up if any.
          e.stopPropagation();
          setFolderDragData(e, node.folder.folder_id);
        }}
        onDragEnd={() => {
          // Always clear the module-local stash so a follow-up drag
          // doesn't see the previous folder id.
          clearActiveFolderDrag();
        }}
        onClick={() => setView(v)}
        style={{ paddingLeft: 22 + depth * 14 }}
        title={`${node.docCount} document${node.docCount === 1 ? "" : "s"}`}
        onDragOver={handleDragOver}
        onDragLeave={clearDropState}
        onDrop={(e) => {
          // Folder reorder takes priority — its MIME is more specific.
          const folderId = readFolderDragData(e) ?? activeFolderDragIdSnapshot();
          // Reject self-drops and drops that would push the folder
          // into its own subtree (cycle).
          const subtree = folderId
            ? descendantIds(allFolders, folderId)
            : null;
          if (
            folderId &&
            subtree &&
            !subtree.has(node.folder.folder_id)
          ) {
            const zone = folderDropZone ?? "into";
            clearDropState();
            clearActiveFolderDrag();
            e.preventDefault();
            e.stopPropagation();
            if (zone === "into") {
              // Reparent into this folder; drop at the end of its
              // children list (no neighbour ids).
              void onReorderFolder(folderId, node.folder.folder_id, null, null);
            } else {
              // Reorder among siblings of this row.
              const beforeId =
                zone === "below"
                  ? node.folder.folder_id
                  : siblingIndex > 0
                    ? siblings[siblingIndex - 1]
                    : null;
              const afterId =
                zone === "above"
                  ? node.folder.folder_id
                  : siblingIndex < siblings.length - 1
                    ? siblings[siblingIndex + 1]
                    : null;
              void onReorderFolder(folderId, parentId, beforeId, afterId);
            }
            return;
          }
          clearDropState();
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
                label: "New subfolder…",
                icon: <Icon name="folder" />,
                onClick: () => onCreateSubfolder(node.folder.folder_id),
              },
              {
                label: "Rename…",
                icon: <Icon name="folder" />,
                onClick: () => onRenameFolder(node.folder),
              },
              {
                label: "Delete folder…",
                icon: <Icon name="trash" />,
                onClick: () => onDeleteFolder(node.folder),
                separatorBefore: true,
              },
            ]}
          />
        </span>
      </li>
      {hasChildren &&
        isOpen &&
        node.children.map((child, idx) => (
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
            onCreateSubfolder={onCreateSubfolder}
            onDeleteFolder={onDeleteFolder}
            onReorderFolder={onReorderFolder}
            allFolders={allFolders}
            siblings={node.children.map((c) => c.folder.folder_id)}
            siblingIndex={idx}
            parentId={node.folder.folder_id}
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
  onMove,
  onTranscribe,
  onViewTranscript,
  onStartExportDrag,
  onPrefetchExport,
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
  onMove: (d: DocumentSummary) => void;
  onTranscribe: (d: DocumentSummary) => void;
  onViewTranscript: (d: DocumentSummary) => void;
  onStartExportDrag: (d: DocumentSummary) => void;
  onPrefetchExport: (documentId: string) => void;
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
                  ? "Click to toggle · ⇧-click for range · drag to move · ⌥-drag to export"
                  : "Click to open · ⌘-click to start selecting · drag to move · ⌥-drag to export"
              }
              draggable
              onDragStart={(e) => {
                // ⌥-drag is the "export out to Finder/Desktop"
                // gesture: cancel the HTML5 drag (so the internal
                // folder-move handlers don't see it) and start a
                // native OS drag-source via tauri-plugin-drag.
                // Without ⌥, the existing internal drag path
                // (folder reorder, batch move) runs unchanged.
                if (e.altKey) {
                  e.preventDefault();
                  onStartExportDrag(d);
                  return;
                }
                const ids = isSelected && selectedIds.size > 1
                  ? [...selectedIds]
                  : [d.document_id];
                setDocumentDragData(e, d.document_id, ids);
                setCustomDragImage(e, d.visible_name, ids.length, DRAG_ICON_SVG);
              }}
              onMouseEnter={() => onPrefetchExport(d.document_id)}
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
                      label: "Move to folder…",
                      icon: <Icon name="folder" />,
                      onClick: () => onMove(d),
                    },
                    {
                      label: "Convert to text…",
                      icon: <Icon name="wand" />,
                      onClick: () => onTranscribe(d),
                      separatorBefore: true,
                    },
                    {
                      label: "View transcript",
                      icon: <Icon name="info" />,
                      onClick: () => onViewTranscript(d),
                    },
                    {
                      label: "Show history",
                      icon: <Icon name="history" />,
                      onClick: () => onOpen(d),
                      separatorBefore: true,
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
  onOpenInViewer,
  onRename,
  onArchive,
  onShowHistory,
  onMove,
  onTranscribe,
  onViewTranscript,
  onStartExportDrag,
  onPrefetchExport,
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
  onOpenInViewer: (d: DocumentSummary) => void;
  onRename: (d: DocumentSummary) => void;
  onArchive: (d: DocumentSummary) => void;
  onShowHistory: (d: DocumentSummary) => void;
  onMove: (d: DocumentSummary) => void;
  onTranscribe: (d: DocumentSummary) => void;
  onViewTranscript: (d: DocumentSummary) => void;
  onStartExportDrag: (d: DocumentSummary) => void;
  onPrefetchExport: (documentId: string) => void;
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
            title="Click to open · drag to move · ⌥-drag to export"
            draggable
            onDragStart={(e) => {
              // See the matching DocumentList handler for the ⌥
              // branch rationale.
              if (e.altKey) {
                e.preventDefault();
                onStartExportDrag(d);
                return;
              }
              const ids =
                isSelected && selectedIds.size > 1
                  ? [...selectedIds]
                  : [d.document_id];
              setDocumentDragData(e, d.document_id, ids);
              setCustomDragImage(e, d.visible_name, ids.length, DRAG_ICON_SVG);
            }}
            onMouseEnter={() => onPrefetchExport(d.document_id)}
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
            <span
              className="tile-kebab"
              onClick={(e) => e.stopPropagation()}
            >
              <Menu
                align="right"
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
                    label: "Move to folder…",
                    icon: <Icon name="folder" />,
                    onClick: () => onMove(d),
                  },
                  {
                    label: "Convert to text…",
                    icon: <Icon name="wand" />,
                    onClick: () => onTranscribe(d),
                    separatorBefore: true,
                  },
                  {
                    label: "View transcript",
                    icon: <Icon name="info" />,
                    onClick: () => onViewTranscript(d),
                  },
                  {
                    label: "Show history",
                    icon: <Icon name="history" />,
                    onClick: () => onShowHistory(d),
                    separatorBefore: true,
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
