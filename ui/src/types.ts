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

export interface OcrModelDescriptor {
  id: string;
  display_name: string;
  size_bytes: number;
}

export type OcrStatusReport =
  | { kind: "missing"; descriptor: OcrModelDescriptor }
  | { kind: "cached"; descriptor: OcrModelDescriptor }
  | { kind: "ready"; descriptor: OcrModelDescriptor };

export type OcrProgressEvent =
  | { kind: "page_started"; page_index: number }
  | { kind: "page_done"; page_index: number; chars: number }
  | { kind: "page_failed"; page_index: number; message: string }
  | { kind: "done"; pages_done: number; total_chars: number }
  | { kind: "download_progress"; done: number; total: number | null }
  | { kind: "model_loading" }
  | { kind: "download_done" };

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
  | { kind: "done"; recorded: number; unchanged: number; skipped: number }
  | { kind: "cancelled" };
