// View definitions + per-view filtering/labeling extracted from
// App.tsx. The sidebar exposes a fixed set of top-level "views"
// (All, Recent, Unsynced, Notebooks, PDFs, EPUBs, Archive) plus
// every user folder. `View` discriminates the two; the helpers here
// turn a `View` into a title, subtitle, filter predicate, and empty
// state.

import type { DocumentSummary, FolderEntry, LibrarySummary } from "./types";

export type View =
  | "all"
  | "recent"
  | "unsynced"
  | "notebooks"
  | "pdfs"
  | "epubs"
  | "archive"
  | "device_trash"
  | "root"
  | { kind: "folder"; id: string };

export function viewKey(v: View): string {
  if (typeof v === "string") return v;
  return `folder:${v.id}`;
}

export function viewTitle(v: View, folders: FolderEntry[]): string {
  if (typeof v === "object") {
    return folders.find((f) => f.folder_id === v.id)?.visible_name ?? "Folder";
  }
  switch (v) {
    case "all":
      return "All Documents";
    case "recent":
      return "Recently Synced";
    case "unsynced":
      return "Pending Sync";
    case "notebooks":
      return "Notebooks";
    case "pdfs":
      return "PDFs";
    case "epubs":
      return "EPUBs";
    case "archive":
      return "Archive";
    case "device_trash":
      return "Tablet Trash";
    case "root":
      return "My Files";
  }
}

export function viewSubtitle(v: View): string {
  if (typeof v === "object") return "Folder";
  switch (v) {
    case "all":
      return "Everything in your library";
    case "recent":
      return "Synced in the last 24 hours";
    case "unsynced":
      return "Will upload to the tablet on next sync";
    case "notebooks":
      return "Handwritten and template-based";
    case "pdfs":
      return "Imported PDFs";
    case "epubs":
      return "Imported EPUBs";
    case "archive":
      return "Items pending deletion · restore at any time";
    case "device_trash":
      return "Documents the tablet has deleted but not yet purged from device storage";
    case "root":
      return "Documents at the top level of your library";
  }
}

const RECENT_WINDOW_MS = 24 * 60 * 60 * 1000;

export function recentCount(docs: DocumentSummary[]): number {
  return filterRecent(docs).length;
}

export function countByKind(
  docs: DocumentSummary[],
  kindMatch: string,
): number {
  return docs.filter((d) => d.doc_type === kindMatch).length;
}

export function filterRecent(docs: DocumentSummary[]): DocumentSummary[] {
  const cutoff = Date.now() - RECENT_WINDOW_MS;
  return docs
    .filter((d) => {
      const t = Date.parse(d.last_observed_at);
      return Number.isFinite(t) && t >= cutoff;
    })
    .sort((a, b) => b.last_observed_at.localeCompare(a.last_observed_at));
}

export function filterDocuments(
  docs: DocumentSummary[],
  view: View,
): DocumentSummary[] {
  if (typeof view === "object") {
    return docs.filter((d) => d.parent === view.id);
  }
  switch (view) {
    case "root":
      return docs.filter((d) => d.parent === null);
    case "all":
      return docs;
    case "recent":
      return filterRecent(docs);
    case "unsynced":
      return docs.filter((d) => d.has_unpushed_changes);
    case "notebooks":
      return docs.filter((d) => d.doc_type === "Notebook");
    case "pdfs":
      return docs.filter((d) => d.doc_type === "DocumentType.Pdf");
    case "epubs":
      return docs.filter((d) => d.doc_type === "DocumentType.Epub");
    case "archive":
      return [];
    case "device_trash":
      return docs.filter((d) => d.parent === "trash");
  }
}

export function unsyncedCount(docs: DocumentSummary[]): number {
  return docs.filter((d) => d.has_unpushed_changes).length;
}

export function emptyHintFor(view: View): { title: string; body: string } {
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
    case "device_trash":
      return {
        title: "Tablet Trash is empty",
        body: "Documents the tablet has soft-deleted (but not yet purged) appear here. Sync to refresh.",
      };
    case "unsynced":
      return {
        title: "Everything is synced",
        body: "When you import or edit, items waiting to upload will appear here.",
      };
    case "root":
      return {
        title: "My Files is empty",
        body: "Documents not inside any folder appear here.",
      };
  }
}

export function summaryHealth(_s: LibrarySummary): string {
  // Without an integrated verify result, treat as healthy by default;
  // proper health colouring lives in a future phase.
  return "";
}

// localStorage round-trip for the last view + viewMode the user was
// on. Folder views (`{kind: "folder", id}`) are skipped on read
// because the folder may not exist after a library switch; we'd
// rather drop the user on "All Documents" than show a phantom empty
// folder.
const LS_VIEW_KEY = "rh.view";
const LS_VIEW_MODE_KEY = "rh.viewMode";
const TOP_LEVEL_VIEWS: ReadonlySet<string> = new Set([
  "all",
  "recent",
  "unsynced",
  "notebooks",
  "pdfs",
  "epubs",
  "archive",
]);

export function loadPersistedView(): View {
  try {
    const raw = window.localStorage.getItem(LS_VIEW_KEY);
    if (raw && TOP_LEVEL_VIEWS.has(raw)) return raw as View;
  } catch {
    // localStorage may throw in private mode or corrupted profiles —
    // fall back to default rather than crash on launch.
  }
  return "all";
}

export function persistView(v: View): void {
  try {
    if (typeof v === "string") window.localStorage.setItem(LS_VIEW_KEY, v);
    // Folder views are intentionally not persisted (see comment above).
  } catch {
    /* ignore */
  }
}

export function loadPersistedViewMode(): "list" | "grid" {
  try {
    const raw = window.localStorage.getItem(LS_VIEW_MODE_KEY);
    if (raw === "list" || raw === "grid") return raw;
  } catch {
    /* ignore */
  }
  return "list";
}

export function persistViewMode(m: "list" | "grid"): void {
  try {
    window.localStorage.setItem(LS_VIEW_MODE_KEY, m);
  } catch {
    /* ignore */
  }
}
