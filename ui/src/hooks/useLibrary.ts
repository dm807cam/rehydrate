// Library content state + the two refresh callbacks, extracted
// from App.tsx.
//
// The hook owns the four content lists (`documents`, `folders`,
// `archived`, `summary`), the open-state pair (`libraryOpen`,
// `libraryPath`), the recents list (`recentLibraries`), and the
// canonical refresh path.
//
// What stays in App.tsx: the imperative orchestrators
// (`openLibrary`, `switchToLibrary`, `openAnotherLibrary`) — those
// reach into selection state, expanded-folder state, and the
// confirm dialog, all of which would make the hook's surface
// unfocused. They consume the hook's setters directly.
//
// Concurrency: `refreshing` marks an in-flight refresh and
// `pendingRefresh` is a single-slot "queue another one after this
// finishes" flag. Most refresh calls fire right after an `await ipc.X()`
// (rename, archive, reorder, multi-select toolbar actions), so a
// second call while one is mid-flight is usually the *load-bearing*
// one — its results reflect the write the user just made. Dropping
// it (issue #38) left the UI showing pre-write state until the next
// user action triggered another refresh. The single-slot debounce
// guarantees the last requested refresh always completes after the
// most recent write, without queueing an unbounded number of
// in-flights.

import { useCallback, useRef, useState } from "react";

import { formatError } from "../formatError";
import { ipc } from "../ipc";
import type {
  ArchivedDocument,
  DocumentSummary,
  FolderEntry,
  LibrarySummary,
  RecentLibraryEntry,
} from "../types";

export interface UseLibraryInjections {
  /// App-level error sink. Refresh failures route through it.
  setError: (msg: string | null) => void;
}

export interface UseLibraryResult {
  libraryOpen: boolean;
  libraryPath: string | null;
  recentLibraries: RecentLibraryEntry[];
  summary: LibrarySummary | null;
  documents: DocumentSummary[] | null;
  folders: FolderEntry[];
  archived: ArchivedDocument[];
  setLibraryOpen: React.Dispatch<React.SetStateAction<boolean>>;
  setLibraryPath: React.Dispatch<React.SetStateAction<string | null>>;
  setRecentLibraries: React.Dispatch<
    React.SetStateAction<RecentLibraryEntry[]>
  >;
  setSummary: React.Dispatch<React.SetStateAction<LibrarySummary | null>>;
  setDocuments: React.Dispatch<
    React.SetStateAction<DocumentSummary[] | null>
  >;
  setFolders: React.Dispatch<React.SetStateAction<FolderEntry[]>>;
  setArchived: React.Dispatch<React.SetStateAction<ArchivedDocument[]>>;
  /// Refetch all four content lists in parallel. Safe under
  /// concurrent calls: a second invocation arriving mid-flight
  /// queues a single re-fetch that runs as soon as the in-flight
  /// one finishes (issue #38 — the second call usually IS the
  /// load-bearing one, since it's typically issued right after a
  /// write IPC returns).
  refreshLibrary: () => Promise<void>;
  /// Refetch only the recents list. Non-fatal on failure — the
  /// switcher just keeps showing whatever it had.
  refreshRecentLibraries: () => Promise<void>;
  /// Wipe in-memory content state. Used before a library switch so
  /// the user sees an obvious "loading" state instead of
  /// cross-library leakage during the switch round-trip.
  clearLibraryUi: () => void;
}

export function useLibrary(injections: UseLibraryInjections): UseLibraryResult {
  // Stash the injections in a ref so the callbacks below don't have
  // to declare them in `useCallback` deps. App-side callers tend to
  // pass inline arrows for the `setError` adapter (`(msg) =>
  // setError(msg)`) which change identity every render; without the
  // ref, every memoized callback in this hook would be re-created
  // on every render, defeating the entire point of memoization.
  const injectionsRef = useRef(injections);
  injectionsRef.current = injections;
  const [libraryOpen, setLibraryOpen] = useState(false);
  const [libraryPath, setLibraryPath] = useState<string | null>(null);
  const [recentLibraries, setRecentLibraries] = useState<RecentLibraryEntry[]>(
    [],
  );
  const [summary, setSummary] = useState<LibrarySummary | null>(null);
  const [documents, setDocuments] = useState<DocumentSummary[] | null>(null);
  const [folders, setFolders] = useState<FolderEntry[]>([]);
  const [archived, setArchived] = useState<ArchivedDocument[]>([]);

  const refreshing = useRef(false);
  // Issue #38: if a second refreshLibrary() arrives while the first
  // is mid-flight, flip this slot and let the in-flight refresh
  // re-run once it completes. Single-slot — a third concurrent call
  // collapses with the second, so we re-run at most once after the
  // latest in-flight finishes.
  const pendingRefresh = useRef(false);

  const refreshLibrary = useCallback(async () => {
    if (refreshing.current) {
      pendingRefresh.current = true;
      return;
    }
    refreshing.current = true;
    try {
      // Loop until no further refresh has been requested mid-flight.
      // Clearing `pendingRefresh` BEFORE the fetch (not after) means
      // any call that arrives between the fetch and the setState
      // batch sets the flag again and we'll round-trip once more —
      // so the last call's IPC results are guaranteed to be the
      // ones rendered.
      while (true) {
        pendingRefresh.current = false;
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
        if (!pendingRefresh.current) break;
      }
    } catch (e) {
      injectionsRef.current.setError(formatError(e));
    } finally {
      refreshing.current = false;
      // Drop any queued refresh on error — the user has been shown
      // the failure and can retry explicitly. Looping after an
      // error would either re-fire the same failure or mask it.
      pendingRefresh.current = false;
    }
  }, []);

  const refreshRecentLibraries = useCallback(async () => {
    try {
      const list = await ipc.listRecentLibraries();
      setRecentLibraries(list);
    } catch {
      // Non-fatal — the switcher just shows whatever it had.
    }
  }, []);

  const clearLibraryUi = useCallback(() => {
    setDocuments(null);
    setFolders([]);
    setArchived([]);
    setSummary(null);
  }, []);

  return {
    libraryOpen,
    libraryPath,
    recentLibraries,
    summary,
    documents,
    folders,
    archived,
    setLibraryOpen,
    setLibraryPath,
    setRecentLibraries,
    setSummary,
    setDocuments,
    setFolders,
    setArchived,
    refreshLibrary,
    refreshRecentLibraries,
    clearLibraryUi,
  };
}
