// Drag-and-drop helpers extracted from App.tsx.
//
// Drop the document/folder id into the DataTransfer under a custom
// MIME type so external drops (a PDF dragged in from Finder) don't
// look like internal drops, and so internal drops can carry a batch
// of selected ids when the user is multi-selecting.
//
// The reorder logic uses float midpoints for sibling sort_index so a
// single reorder doesn't have to renumber every sibling. The same
// pattern reMarkable uses in its own metadata layout.

import type { DragEvent as ReactDragEvent } from "react";

import type { FolderEntry } from "./types";

const DOC_DRAG_MIME = "application/x-rehydrate-doc";
const DOC_DRAG_BATCH_MIME = "application/x-rehydrate-doc-batch";
const FOLDER_DRAG_MIME = "application/x-rehydrate-folder";

export function setDocumentDragData(
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

export function readDocumentDragData(
  e: ReactDragEvent,
): { id: string; batch: string[] } | null {
  const id = e.dataTransfer.getData(DOC_DRAG_MIME);
  if (!id) return null;
  const batchRaw = e.dataTransfer.getData(DOC_DRAG_BATCH_MIME);
  const batch = batchRaw ? batchRaw.split(",").filter(Boolean) : [id];
  return { id, batch };
}

export function hasDocumentDragData(e: ReactDragEvent): boolean {
  return e.dataTransfer.types.includes(DOC_DRAG_MIME);
}

// HTML5 drag-and-drop puts the DataTransfer into "protected mode"
// during `dragover`, so `getData(...)` returns "" until the user
// actually drops. We need the dragged folder id during `dragover` (to
// size the drop zones, to short-circuit invalid targets so the browser
// shows a "no entry" cursor, and crucially to call `preventDefault()`
// only when the drop would be valid). Stash the id in a module-local
// on `dragstart` and clear it on `dragend` — same pattern several DnD
// libs use.
let activeFolderDragId: string | null = null;

export function activeFolderDragIdSnapshot(): string | null {
  return activeFolderDragId;
}

export function setFolderDragData(e: ReactDragEvent, folderId: string) {
  e.dataTransfer.setData(FOLDER_DRAG_MIME, folderId);
  e.dataTransfer.setData("text/plain", folderId);
  e.dataTransfer.effectAllowed = "move";
  activeFolderDragId = folderId;
}

export function clearActiveFolderDrag() {
  activeFolderDragId = null;
}

export function readFolderDragData(e: ReactDragEvent): string | null {
  const id = e.dataTransfer.getData(FOLDER_DRAG_MIME);
  return id || null;
}

export function hasFolderDragData(e: ReactDragEvent): boolean {
  return e.dataTransfer.types.includes(FOLDER_DRAG_MIME);
}

/// Returns the set of folder ids that include `rootId` and every
/// folder transitively parented by it. Used to forbid moving a folder
/// into its own subtree (which would create a cycle the backend would
/// reject anyway, but we filter at the UI to avoid the round trip and
/// an error toast).
export function descendantIds(
  folders: FolderEntry[],
  rootId: string,
): Set<string> {
  const childrenOf = new Map<string, string[]>();
  for (const f of folders) {
    if (!f.parent) continue;
    const list = childrenOf.get(f.parent) ?? [];
    list.push(f.folder_id);
    childrenOf.set(f.parent, list);
  }
  const out = new Set<string>([rootId]);
  const stack = [rootId];
  while (stack.length > 0) {
    const cur = stack.pop()!;
    for (const child of childrenOf.get(cur) ?? []) {
      if (!out.has(child)) {
        out.add(child);
        stack.push(child);
      }
    }
  }
  return out;
}

/// Compute the sort_index needed to slot `dragged` adjacent to
/// `target` (or at end of `parent`'s children for `into`) inside the
/// existing folder list. Uses midpoints of float keys so we don't have
/// to renumber siblings on every drag.
export function computeReorderSortIndex(
  folders: FolderEntry[],
  draggedId: string,
  newParent: string | null,
  beforeId: string | null,
  afterId: string | null,
): number {
  const siblings = folders
    .filter(
      (f) =>
        (f.parent ?? null) === newParent && f.folder_id !== draggedId,
    )
    .slice()
    .sort((a, b) => {
      const cmp = (a.sort_index ?? 0) - (b.sort_index ?? 0);
      if (cmp !== 0) return cmp;
      return a.visible_name.localeCompare(b.visible_name);
    });
  const before = beforeId
    ? siblings.find((f) => f.folder_id === beforeId)
    : null;
  const after = afterId
    ? siblings.find((f) => f.folder_id === afterId)
    : null;
  if (before && after) return (before.sort_index + after.sort_index) / 2;
  if (before) return before.sort_index + 1;
  if (after) return after.sort_index - 1;
  return 0;
}

