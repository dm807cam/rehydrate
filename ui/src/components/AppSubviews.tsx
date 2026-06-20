//! Sub-components and pure helpers extracted from App.tsx so the App
//! shell stays focused on orchestration. Everything in this file is
//! stateless from App.tsx's perspective: each component receives its
//! state and callbacks via props, with internal `useState`/`useRef`
//! only for genuinely component-local UI state (drag hover, sort
//! prefs, anchor id for shift-click).
//!
//! Public exports (consumed by App.tsx):
//!   - WelcomeEmpty, SidebarItem, FolderTree
//!   - DocumentList, DocumentGrid, ArchiveList, DocumentListSkeleton
//!   - prettyType, formatBytes
//!
//! Internal helpers (file-private):
//!   - SortCol / SortDir, loadSortPref, sortDocuments
//!   - FolderTreeNode, FolderReorder, buildFolderTree, FolderRow
//!   - TypeIcon, prettyDate

import {
  useEffect,
  useRef,
  useState,
  type DragEvent as ReactDragEvent,
  type ReactNode,
} from "react";

import { Icon } from "./Icon";
import { Menu } from "./Menu";
import { Skeleton } from "./Skeleton";
import { Thumbnail } from "./Thumbnail";

import {
  activeFolderDragIdSnapshot,
  clearActiveFolderDrag,
  descendantIds,
  hasDocumentDragData,
  hasFolderDragData,
  readDocumentDragData,
  readFolderDragData,
  setDocumentDragData,
  setFolderDragData,
} from "../drag";
import { DRAG_ICON_SVG, setCustomDragImage } from "../dragImage";
import type {
  ArchivedDocument,
  DocumentSummary,
  FolderEntry,
} from "../types";
import { type View, viewKey } from "../views";


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

export function WelcomeEmpty({
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

export function SidebarItem({
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

export function FolderTree({
  folders,
  documents,
  view,
  setView,
  expanded,
  setExpanded,
  onDocumentDrop,
  onExternalFileDrop,
  onRenameFolder,
  onExportFolder,
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
  onExternalFileDrop: (files: File[], folderId: string) => void;
  onRenameFolder: (f: FolderEntry) => void;
  onExportFolder: (f: FolderEntry) => void;
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
          onExternalFileDrop={onExternalFileDrop}
          onRenameFolder={onRenameFolder}
          onExportFolder={onExportFolder}
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
  onExternalFileDrop,
  onRenameFolder,
  onExportFolder,
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
  onExternalFileDrop: (files: File[], folderId: string) => void;
  onRenameFolder: (f: FolderEntry) => void;
  onExportFolder: (f: FolderEntry) => void;
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
    const hasFiles = Array.from(e.dataTransfer.types).includes("Files");
    if (!hasDocumentDragData(e) && !hasFiles) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "copy";
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
          // OS file drop (from Finder / Explorer): import into this folder.
          const files = Array.from(e.dataTransfer.files);
          if (files.length > 0) {
            e.preventDefault();
            e.stopPropagation();
            onExternalFileDrop(files, node.folder.folder_id);
            return;
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
          {hasChildren && (
            <svg
              width="10" height="10" viewBox="0 0 10 10"
              fill="none" stroke="currentColor"
              strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"
              className={`folder-chevron${isOpen ? " open" : ""}`}
            >
              <path d="M3 1.5 L7 5 L3 8.5" />
            </svg>
          )}
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
                label: "Export to PDFs…",
                icon: <Icon name="arrowUp" />,
                onClick: () => onExportFolder(node.folder),
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
            onExternalFileDrop={onExternalFileDrop}
            onRenameFolder={onRenameFolder}
            onExportFolder={onExportFolder}
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

export function DocumentListSkeleton() {
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

type SortCol = "title" | "type" | "size" | "pages" | "synced";
type SortDir = "asc" | "desc";

const LS_SORT_COL: string = "rh.sortCol";
const LS_SORT_DIR: string = "rh.sortDir";

function loadSortPref(): { col: SortCol; dir: SortDir } {
  try {
    const col = window.localStorage.getItem(LS_SORT_COL) as SortCol | null;
    const dir = window.localStorage.getItem(LS_SORT_DIR) as SortDir | null;
    const validCols: SortCol[] = ["title", "type", "size", "pages", "synced"];
    if (col && validCols.includes(col) && (dir === "asc" || dir === "desc")) {
      return { col, dir };
    }
  } catch { /* ignore */ }
  return { col: "title", dir: "asc" };
}

function sortDocuments(
  docs: DocumentSummary[],
  col: SortCol,
  dir: SortDir,
): DocumentSummary[] {
  const sign = dir === "asc" ? 1 : -1;
  return [...docs].sort((a, b) => {
    switch (col) {
      case "title":
        return sign * a.visible_name.localeCompare(b.visible_name, undefined, { sensitivity: "base" });
      case "type":
        return sign * a.doc_type.localeCompare(b.doc_type);
      case "size":
        return sign * (a.size_bytes - b.size_bytes);
      case "pages": {
        // nulls always sort last regardless of direction
        if (a.page_count === null && b.page_count === null) return 0;
        if (a.page_count === null) return 1;
        if (b.page_count === null) return -1;
        return sign * (a.page_count - b.page_count);
      }
      case "synced":
        return sign * a.last_observed_at.localeCompare(b.last_observed_at);
    }
  });
}

export function DocumentList({
  documents,
  selectedIds,
  selectMode,
  focusId,
  onClickRow,
  onSelectAll,
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
    list: DocumentSummary[],
    e: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean },
  ) => void;
  onSelectAll: () => void;
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
  const [{ col: sortCol, dir: sortDir }, setSortPref] = useState<{
    col: SortCol;
    dir: SortDir;
  }>(loadSortPref);

  function handleSortClick(col: SortCol) {
    setSortPref((prev) => {
      const dir: SortDir =
        prev.col === col && prev.dir === "asc" ? "desc" : "asc";
      try {
        window.localStorage.setItem(LS_SORT_COL, col);
        window.localStorage.setItem(LS_SORT_DIR, dir);
      } catch { /* ignore */ }
      return { col, dir };
    });
  }

  const sorted = sortDocuments(documents, sortCol, sortDir);
  const arrow = sortDir === "asc" ? " ↑" : " ↓";

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
          {selectMode && (
            <th className="check-cell">
              <span
                className={`check-box${
                  documents.length > 0 && documents.every((d) => selectedIds.has(d.document_id))
                    ? " checked"
                    : selectedIds.size > 0
                      ? " indeterminate"
                      : ""
                }`}
                role="checkbox"
                aria-label="Select all"
                onClick={(e) => { e.preventDefault(); e.stopPropagation(); onSelectAll(); }}
              >
                {documents.length > 0 && documents.every((d) => selectedIds.has(d.document_id)) ? (
                  <Icon name="checkboxChecked" size={16} />
                ) : selectedIds.size > 0 ? (
                  <Icon name="checkboxIndeterminate" size={16} />
                ) : (
                  <Icon name="checkbox" size={16} />
                )}
              </span>
            </th>
          )}
          <th
            className={`sortable${sortCol === "title" ? " sort-active" : ""}`}
            onClick={() => handleSortClick("title")}
          >
            Title{sortCol === "title" ? arrow : ""}
          </th>
          <th
            className={`sortable${sortCol === "type" ? " sort-active" : ""}`}
            onClick={() => handleSortClick("type")}
          >
            Type{sortCol === "type" ? arrow : ""}
          </th>
          <th
            className={`sortable${sortCol === "size" ? " sort-active" : ""}`}
            onClick={() => handleSortClick("size")}
          >
            Size{sortCol === "size" ? arrow : ""}
          </th>
          <th
            className={`sortable${sortCol === "pages" ? " sort-active" : ""}`}
            onClick={() => handleSortClick("pages")}
          >
            Pages{sortCol === "pages" ? arrow : ""}
          </th>
          <th
            className={`sortable${sortCol === "synced" ? " sort-active" : ""}`}
            onClick={() => handleSortClick("synced")}
          >
            Synced{sortCol === "synced" ? arrow : ""}
          </th>
          <th></th>
        </tr>
      </thead>
      <tbody>
        {sorted.map((d) => {
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
              onClick={(e) => {
                // Prevent browser text selection when shift+clicking rows.
                if (e.shiftKey) e.preventDefault();
                onClickRow(d, sorted, {
                  metaKey: e.metaKey,
                  ctrlKey: e.ctrlKey,
                  shiftKey: e.shiftKey,
                });
              }}
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

export function DocumentGrid({
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
    list: DocumentSummary[],
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
            onClick={(e) => {
              if (e.shiftKey) e.preventDefault();
              onClickRow(d, documents, {
                metaKey: e.metaKey,
                ctrlKey: e.ctrlKey,
                shiftKey: e.shiftKey,
              });
            }}
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

export function ArchiveList({
  archived,
  onRestore,
  onPurge,
  onShowHistory,
  emptyHint,
}: {
  archived: ArchivedDocument[];
  onRestore: (docs: ArchivedDocument[]) => void;
  onPurge: (docs: ArchivedDocument[]) => void;
  onShowHistory: (d: ArchivedDocument) => void;
  emptyHint: { title: string; body: string };
}) {
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [anchorId, setAnchorId] = useState<string | null>(null);

  // Reset selection when the list changes (e.g. after restore/purge).
  useEffect(() => {
    setSelectedIds((prev) => {
      const ids = new Set(archived.map((d) => d.document_id));
      const next = new Set([...prev].filter((id) => ids.has(id)));
      return next.size === prev.size ? prev : next;
    });
  }, [archived]);

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

  const allSelected =
    archived.length > 0 && archived.every((d) => selectedIds.has(d.document_id));
  const someSelected = selectedIds.size > 0;

  function toggleRow(d: ArchivedDocument, e: { shiftKey: boolean; preventDefault(): void }) {
    e.preventDefault();
    const id = d.document_id;
    if (e.shiftKey && anchorId) {
      const ai = archived.findIndex((x) => x.document_id === anchorId);
      const bi = archived.findIndex((x) => x.document_id === id);
      if (ai >= 0 && bi >= 0) {
        const [lo, hi] = ai < bi ? [ai, bi] : [bi, ai];
        setSelectedIds(
          new Set(archived.slice(lo, hi + 1).map((x) => x.document_id)),
        );
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
  }

  function toggleAll() {
    if (allSelected) {
      setSelectedIds(new Set());
    } else {
      setSelectedIds(new Set(archived.map((d) => d.document_id)));
    }
  }

  const selectedDocs = archived.filter((d) => selectedIds.has(d.document_id));

  return (
    <>
      <div className="banner">
        <Icon name="info" />
        <span>
          Items here will leave the tablet on the next sync.{" "}
          <strong>Restore</strong> brings them back;{" "}
          <strong>Delete forever</strong> drops every saved version and cannot
          be undone.
        </span>
      </div>
      <div className="archive-toolbar">
        <span className="selected-hint">
          {someSelected ? (
            <strong>{selectedIds.size}</strong>
          ) : (
            <span className="muted">0</span>
          )}{" "}
          of {archived.length} selected
        </span>
        <div className="spacer" />
        <button
          type="button"
          onClick={(e) => { e.preventDefault(); onRestore(selectedDocs); }}
          disabled={!someSelected}
          title="Restore selected documents to the library."
        >
          <Icon name="restore" />
          {" "}Restore{someSelected ? ` (${selectedIds.size})` : ""}
        </button>
        <button
          type="button"
          className="danger"
          onClick={(e) => { e.preventDefault(); onPurge(selectedDocs); }}
          disabled={!someSelected}
          title="Permanently delete all versions of selected documents."
        >
          <Icon name="delete" />
          {" "}Delete forever{someSelected ? ` (${selectedIds.size})` : ""}
        </button>
      </div>
      <table className="docs select-mode">
        <thead>
          <tr>
            <th className="check-cell">
              <span
                className={`check-box${allSelected ? " checked" : someSelected ? " indeterminate" : ""}`}
                role="checkbox"
                aria-label="Select all"
                onClick={(e) => { e.preventDefault(); e.stopPropagation(); toggleAll(); }}
              >
                {allSelected ? (
                  <Icon name="checkboxChecked" size={16} />
                ) : someSelected ? (
                  <Icon name="checkboxIndeterminate" size={16} />
                ) : (
                  <Icon name="checkbox" size={16} />
                )}
              </span>
            </th>
            <th>Title</th>
            <th>Type</th>
            <th>Reason</th>
            <th>Archived</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {archived.map((d) => {
            const isSelected = selectedIds.has(d.document_id);
            return (
              <tr
                key={d.document_id}
                className={`clickable${isSelected ? " selected" : ""}`}
                onClick={(e) => toggleRow(d, e)}
              >
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
                <td className="row-title">
                  <span className="type-glyph">
                    <TypeIcon kind={d.doc_type} />
                  </span>
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
                        label: "Restore",
                        icon: <Icon name="restore" />,
                        onClick: () => onRestore([d]),
                      },
                      {
                        label: "Delete forever",
                        icon: <Icon name="delete" />,
                        onClick: () => onPurge([d]),
                        danger: true,
                        separatorBefore: true,
                      },
                      {
                        label: "Show history",
                        icon: <Icon name="history" />,
                        onClick: () => onShowHistory(d),
                      },
                    ]}
                  />
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </>
  );
}

// =====================================================================
// Formatting
// =====================================================================

export function prettyType(s: string): string {
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

export function formatBytes(n: number): string {
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
