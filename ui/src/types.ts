export interface DocumentSummary {
  document_id: string;
  visible_name: string;
  doc_type: string;
  current_manifest: string;
  current_version_id: number;
  last_observed_at: string;
  parent: string | null;
  size_bytes: number;
  page_count: number | null;
  has_unpushed_changes: boolean;
}

export interface FolderEntry {
  folder_id: string;
  parent: string | null;
  visible_name: string;
  /** Local-only ordering hint within the parent's children. Lower
   *  comes first; ties break alphabetically. Not synced to the device. */
  sort_index: number;
}

/** Tally of children that were lifted out of a deleted folder.
 *  Mirrors `rehydrate_core::DeleteFolderOutcome`. The UI uses this
 *  to phrase the post-delete toast precisely. */
export interface DeleteFolderOutcome {
  folders_moved: number;
  documents_moved: number;
}

/** Summary of what "Revert unpushed changes" undid. Mirrors
 *  `rehydrate_core::RevertReport`. Imports are NOT counted —
 *  revert deliberately leaves locally-imported documents alone so
 *  the user doesn't lose a fresh PDF when reverting an unrelated
 *  folder edit. */
export interface RevertReport {
  folders_restored: number;
  folders_dropped: number;
  documents_rolled_back: number;
}

export type ArchiveReason = "local" | "device";

export interface ArchivedDocument {
  document_id: string;
  visible_name: string;
  doc_type: string;
  parent: string | null;
  manifest_hash: string;
  version_id: number;
  reason: ArchiveReason;
  archived_at: string;
}

export interface VersionEntry {
  id: number;
  document_id: string;
  manifest_hash: string;
  parent_version_id: number | null;
  observed_at: string;
  source: "pulled" | "imported" | "restored";
  note: string | null;
  total_size_bytes: number | null;
  file_count: number | null;
}

export interface ExportResult {
  path: string;
  file_count: number;
}

/// Paths returned by `ipc.prepareExportPdf`. `file` is the staged
/// document (passed as the `item` to the native drag-out plugin so
/// the OS sees a real file drop), `icon` is the drag-preview PNG
/// the plugin requires alongside it.
export interface ExportDragPaths {
  file: string;
  icon: string;
}

export interface LibrarySummary {
  path: string;
  document_count: number;
  version_count: number;
  blob_count: number;
  size_bytes: number;
}

export interface RecentLibraryEntry {
  path: string;
  label: string;
  last_opened: string;
  available: boolean;
  current: boolean;
}

export interface PickedLibraryDirectory {
  path: string;
  /** "empty" → directory exists but has no library.json yet (caller
   *  should prompt the user "create here?" before opening). "existing"
   *  → already a stamped reHydrate library; open without further prompt. */
  kind: "empty" | "existing";
}

export interface DeviceInfo {
  model: string;
  serial: string | null;
  software_version: string | null;
}

export interface DeviceState {
  reachable: boolean;
  connected: boolean;
  info: DeviceInfo | null;
  has_stored_password: boolean;
  has_recorded_host_key: boolean;
}

export type RemoteEntryKind = "folder" | "document";

export interface RemoteEntry {
  uuid: string;
  visible_name: string;
  doc_type: string;
  parent: string | null;
  kind: RemoteEntryKind;
  device_mtime_hint: string | null;
  metadata: unknown;
}

export type PlanItemStatus = "new" | "changed" | "unchanged" | "skipped";

export interface DocumentPlan {
  entry: RemoteEntry;
  status: PlanItemStatus;
  reason: string | null;
}

export interface PullPlan {
  items: DocumentPlan[];
}

export interface SyncReport {
  recorded: number;
  unchanged: number;
  skipped: number;
}

export interface PushReport {
  pushed: number;
  unchanged: number;
  skipped: number;
}

export interface TwoWayReport {
  pull: SyncReport;
  push: PushReport;
}

export type PushItemStatus = "outbound" | "unchanged" | "skipped";

export interface PushItem {
  document: DocumentSummary;
  status: PushItemStatus;
  reason: string | null;
}

export interface PushPlan {
  items: PushItem[];
}

export interface VerifyReport {
  manifests_total: number;
  manifests_ok: number;
  manifests_missing: number;
  manifests_invalid: number;
  blobs_total: number;
  blobs_missing: number;
  blobs_orphan: number;
  blobs_corrupted: number;
  missing_examples: string[];
  orphan_examples: string[];
}

export interface GarbageCollectReport {
  scanned: number;
  deleted: number;
  bytes_freed: number;
  errors: number;
}

export interface LogTail {
  lines: string[];
  log_dir: string | null;
}

export type ProgressEvent =
  | { kind: "plan_ready"; total_documents: number }
  | { kind: "document_started"; document_id: string; visible_name: string }
  | { kind: "file_fetched"; document_id: string; file: string; bytes: number; deduped: boolean }
  | { kind: "document_completed"; document_id: string; unchanged: boolean }
  | { kind: "document_skipped"; document_id: string; reason: string }
  | { kind: "warning"; message: string }
  | { kind: "done"; recorded: number; unchanged: number; skipped: number }
  | { kind: "cancelled" };

// =====================================================================
// OCR + CMS
// =====================================================================

export interface OllamaConfig {
  base_url: string;
  model: string;
  /** When true, the app auto-transcribes every notebook without a
   *  transcript on app startup. Off by default — opt-in via the
   *  Settings modal's Ollama tab. */
  auto_ocr_on_startup: boolean;
}

/** A document the auto-OCR sweep should transcribe — i.e. a live
 *  notebook whose current version has no `ocr/transcript.md`. */
export interface OcrCandidate {
  document_id: string;
  visible_name: string;
}

export interface CuratedOllamaModel {
  id: string;
  label: string;
  vram_hint: string;
}

export interface PingReport {
  ok: boolean;
  error: string | null;
  models: string[];
}

export type OcrStatusReport =
  | {
      kind: "unreachable";
      base_url: string;
      model: string;
      error: string;
    }
  | {
      kind: "model_missing";
      base_url: string;
      model: string;
      available: string[];
    }
  | {
      kind: "ready";
      base_url: string;
      model: string;
    };

/** OCR progress events streamed from the Ollama backend. */
export type OcrProgressEvent =
  | { kind: "page_started"; page_index: number }
  | { kind: "page_done"; page_index: number; chars: number }
  | { kind: "page_failed"; page_index: number; message: string }
  | { kind: "done"; pages_done: number; total_chars: number };

/// Local UI state describing a running OCR job. Used by `OcrJobChip`
/// and by `App.tsx`. Domain shape, not a component-internal type —
/// lives here so unrelated callers don't reach into a component file
/// to import it.
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

/// Sweep progress, used when the user kicked off "OCR every notebook
/// without a transcript at startup". The chip surfaces "(N of M)"
/// batch progress and re-labels the close affordance to reflect that
/// pressing it stops the sweep instead of just hiding the chip.
export interface OcrSweepProgress {
  totalAtStart: number;
  done: number;
}

export interface TranscriptSummary {
  document_id: string;
  version_id: number;
  page_count: number;
  char_count: number;
  model: string;
}

export interface TranscriptDocument {
  document_id: string;
  version_id: number;
  markdown: string;
  model: string | null;
  created_at: string | null;
  language: string | null;
}

export type ExportFormat = "txt" | "markdown";
export type PublishKind = "ghost" | "wordpress";

export interface PublishResult {
  post_id: string;
  edit_url: string;
  target: PublishKind;
}

export interface PublishCredentialStatus {
  ghost: boolean;
  wordpress: boolean;
}

export interface GhostCredentials {
  base_url: string;
  admin_api_key: string;
}

export interface WordpressCredentials {
  base_url: string;
  username: string;
  application_password: string;
}
