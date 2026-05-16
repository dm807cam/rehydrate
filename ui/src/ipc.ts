import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  ArchivedDocument,
  CuratedOllamaModel,
  DeleteFolderOutcome,
  DeviceInfo,
  DeviceState,
  DocumentSummary,
  ExportDragPaths,
  ExportFormat,
  ExportResult,
  FolderEntry,
  GarbageCollectReport,
  GhostCredentials,
  LibrarySummary,
  LogTail,
  OcrCandidate,
  OcrProgressEvent,
  OcrStatusReport,
  OllamaConfig,
  PickedLibraryDirectory,
  RevertReport,
  PingReport,
  ProgressEvent,
  PublishCredentialStatus,
  PublishKind,
  PublishResult,
  PullPlan,
  PushPlan,
  PushReport,
  RecentLibraryEntry,
  SyncReport,
  TranscriptDocument,
  TranscriptSummary,
  TwoWayReport,
  VerifyReport,
  VersionEntry,
  WordpressCredentials,
} from "./types";

/// Renderer-side cap for `ipc.importDroppedFile`. MUST stay in sync
/// with `MAX_IMPORT_FILE_BYTES` in `crates/rehydrate-app/src/commands.rs`.
/// The renderer pre-flights `file.size` against this so an oversize
/// PDF/EPUB drop never reaches `file.arrayBuffer()` — without the
/// preflight, the renderer would allocate hundreds of MB and the
/// JSON-IPC encoder another ~4× that before the backend rejected it,
/// freezing or crashing the UI (issue #24).
export const MAX_IMPORT_FILE_BYTES = 64 * 1024 * 1024;

export const ipc = {
  ping: () => invoke<string>("ping"),

  defaultLibraryPath: () => invoke<string | null>("default_library_path"),
  openLibrary: (path: string) => invoke<void>("open_library", { path }),
  autoOpenLibrary: () => invoke<string | null>("auto_open_library"),
  switchLibrary: (path: string) => invoke<string>("switch_library", { path }),
  pickLibraryDirectory: () =>
    invoke<PickedLibraryDirectory | null>("pick_library_directory"),
  listRecentLibraries: () =>
    invoke<RecentLibraryEntry[]>("list_recent_libraries"),
  librarySummary: () => invoke<LibrarySummary>("library_summary"),
  listDocuments: () => invoke<DocumentSummary[]>("list_documents"),
  listFolders: () => invoke<FolderEntry[]>("list_folders"),
  listArchived: () => invoke<ArchivedDocument[]>("list_archived"),
  moveDocument: (documentId: string, parentId: string | null) =>
    invoke<void>("move_document", { documentId, parentId }),
  renameDocument: (documentId: string, newName: string) =>
    invoke<void>("rename_document", { documentId, newName }),
  renameFolder: (folderId: string, newName: string) =>
    invoke<void>("rename_folder", { folderId, newName }),
  createFolder: (visibleName: string, parentId: string | null) =>
    invoke<FolderEntry>("create_folder", { visibleName, parentId }),
  deleteFolder: (folderId: string) =>
    invoke<DeleteFolderOutcome>("delete_folder", { folderId }),
  revertUnpushedChanges: () =>
    invoke<RevertReport>("revert_unpushed_changes"),
  reorderFolder: (
    folderId: string,
    newParent: string | null,
    newSortIndex: number,
  ) =>
    invoke<void>("reorder_folder", { folderId, newParent, newSortIndex }),
  archiveDocument: (documentId: string) =>
    invoke<void>("archive_document", { documentId }),
  unarchiveDocument: (documentId: string) =>
    invoke<DocumentSummary>("unarchive_document", { documentId }),
  purgeArchivedDocument: (documentId: string) =>
    invoke<void>("purge_archived_document", { documentId }),
  openDocument: (documentId: string) =>
    invoke<string>("open_document", { documentId }),
  /// Warm the OS-drag staging cache for a document so a subsequent
  /// `startExportDrag` returns instantly. Idempotent and
  /// content-keyed: a cache hit skips the blob read / render entirely.
  /// JS treats the returned paths as opaque — the actual drag is
  /// started by `startExportDrag`, which never returns a path so a
  /// compromised renderer can't substitute one.
  prepareExportPdf: (documentId: string) =>
    invoke<ExportDragPaths>("prepare_export_pdf", { documentId }),
  /// Begin a native OS drag-out for the given document. The backend
  /// stages the file under the export cache (re-using the prefetch
  /// cache when warm) and hands the staged path to the platform
  /// drag-source layer. The renderer never sees the path — the
  /// security model is "JS supplies a document_id, the backend
  /// decides which file leaves the sandbox." macOS-only; other
  /// platforms return an error so the UI's ⌥-drag affordance has a
  /// belt-and-braces backstop on top of the client-side platform gate.
  startExportDrag: (documentId: string) =>
    invoke<void>("start_export_drag", { documentId }),
  /// Right-click "Export PDF…" path: stage the document and copy
  /// it into a folder the user picks via the system dialog.
  /// Returns the absolute path that was written, or `null` if the
  /// user cancelled the folder picker. Works on every platform
  /// (no native-drag dependency); this is the canonical export
  /// path on Windows/Linux.
  exportDocumentPdf: (documentId: string) =>
    invoke<string | null>("export_document_pdf", { documentId }),
  documentThumbnail: (documentId: string) =>
    invoke<string | null>("document_thumbnail", { documentId }),
  getHistory: (documentId: string) =>
    invoke<VersionEntry[]>("get_history", { documentId }),
  setVersionNote: (versionId: number, note: string | null) =>
    invoke<void>("set_version_note", { versionId, note }),
  exportVersion: (versionId: number) =>
    invoke<ExportResult | null>("export_version", { versionId }),
  verifyLibrary: () => invoke<VerifyReport>("verify_library"),
  importFile: () => invoke<DocumentSummary | null>("import_file"),
  importDroppedFile: (fileName: string, bytes: Uint8Array) =>
    // Tauri 2's JSON IPC marshals a plain `number[]` straight into
    // `Vec<u8>` on the Rust side. We pay a per-byte JSON serialisation
    // cost (a few hundred ms for a 100 MB PDF) but avoid pulling in a
    // base64 dependency on either side. If this ever shows up in a
    // profile, switch to a `tauri::ipc::Channel<Vec<u8>>` for a true
    // streaming path.
    invoke<DocumentSummary>("import_dropped_file", {
      fileName,
      bytes: Array.from(bytes),
    }),
  garbageCollect: () => invoke<GarbageCollectReport>("garbage_collect"),
  getRecentLogs: (maxLines?: number) =>
    invoke<LogTail>("get_recent_logs", { maxLines: maxLines ?? null }),

  deviceState: () => invoke<DeviceState>("device_state"),
  saveDevicePassword: (password: string) =>
    invoke<void>("save_device_password", { password }),
  forgetDevicePassword: () => invoke<void>("forget_device_password"),
  /** Clear the pinned host-key fingerprint for the device endpoint.
   *  Resolves with `true` if an entry was removed, `false` if none
   *  existed — both leave the user in the desired "no pinned key"
   *  post-state, so the UI treats them identically. */
  forgetDeviceHostKey: () => invoke<boolean>("forget_device_host_key"),
  connectDevice: (password?: string, remember?: boolean) =>
    invoke<DeviceInfo>("connect_device", {
      password: password ?? null,
      // The backend defaults to *not* persisting unless the renderer
      // explicitly asks. `undefined` ⇒ null on the wire ⇒ no save.
      remember: remember ?? null,
    }),
  disconnectDevice: () => invoke<void>("disconnect_device"),

  pullPlan: () => invoke<PullPlan>("pull_plan"),
  pullExecute: () => invoke<SyncReport>("pull_execute"),
  pushPlan: () => invoke<PushPlan>("push_plan"),
  pushExecute: () => invoke<PushReport>("push_execute"),
  syncTwoWay: () => invoke<TwoWayReport>("sync_two_way"),
  restoreVersion: (versionId: number) =>
    invoke<number>("restore_version", { versionId }),

  // ---- OCR / Ollama ---------------------------------------------------
  ocrStatus: () => invoke<OcrStatusReport>("ocr_status"),
  transcribeDocument: (documentId: string, language: string | null) =>
    invoke<TranscriptSummary>("transcribe_document", {
      documentId,
      language,
    }),
  getTranscript: (versionId: number) =>
    invoke<TranscriptDocument | null>("get_transcript", { versionId }),
  exportTranscript: (versionId: number, format: ExportFormat) =>
    invoke<{ path: string } | null>("export_transcript", {
      versionId,
      format,
    }),
  getOllamaConfig: () => invoke<OllamaConfig>("get_ollama_config"),
  saveOllamaConfig: (cfg: OllamaConfig) =>
    invoke<void>("save_ollama_config", { cfg }),
  pingOllama: (baseUrl: string) =>
    invoke<PingReport>("ping_ollama", { baseUrl }),
  listCuratedOllamaModels: () =>
    invoke<CuratedOllamaModel[]>("list_curated_ollama_models"),
  defaultOllamaModel: () => invoke<string>("default_ollama_model"),
  listDocumentsNeedingOcr: () =>
    invoke<OcrCandidate[]>("list_documents_needing_ocr"),

  // ---- CMS publish ----------------------------------------------------
  publishTranscript: (versionId: number, target: PublishKind) =>
    invoke<PublishResult>("publish_transcript", { versionId, target }),
  /** Open a Ghost/WordPress draft URL in the user's default browser,
   *  allowlisted against the saved publish credentials' host for
   *  `target`. Throws if the URL host doesn't match. */
  openPublishUrl: (url: string, target: PublishKind) =>
    invoke<void>("open_publish_url", { url, target }),
  publishCredentialStatus: () =>
    invoke<PublishCredentialStatus>("publish_credential_status"),
  pingPublishTarget: (target: PublishKind) =>
    invoke<void>("ping_publish_target", { target }),
  setGhostCredentials: (creds: GhostCredentials) =>
    invoke<void>("set_ghost_credentials", { creds }),
  forgetGhostCredentials: () => invoke<void>("forget_ghost_credentials"),
  setWordpressCredentials: (creds: WordpressCredentials) =>
    invoke<void>("set_wordpress_credentials", { creds }),
  forgetWordpressCredentials: () =>
    invoke<void>("forget_wordpress_credentials"),

  /// About / support actions. The Rust side hard-codes the support
  /// URL prefix so a compromised renderer can't open arbitrary URLs
  /// via this command.
  openSupportUrl: (url: string) => invoke<void>("open_support_url", { url }),
  appVersion: () => invoke<string>("app_version"),

  /// Cooperative cancellation. The engines poll the shared flag
  /// between documents (and between OCR pages), so calling either
  /// of these aborts at the next granular boundary — not
  /// instantly, but bounded.
  cancelSync: () => invoke<void>("cancel_sync"),
  cancelOcr: () => invoke<void>("cancel_ocr"),

  /// Reveal the rolling-log directory in the OS file manager. The
  /// Rust side picks the path; the renderer can't influence which
  /// directory gets opened. Returns the resolved path so the UI
  /// can present a fallback if the open call fails.
  revealLogDir: () => invoke<string>("reveal_log_dir"),
};

/** Subscribe to OCR-progress events emitted from the Rust side. */
export function onOcrProgress(
  cb: (ev: OcrProgressEvent) => void,
): Promise<UnlistenFn> {
  return listen<OcrProgressEvent>("ocr:progress", (e) => cb(e.payload));
}

export function onDeviceReachable(
  cb: (reachable: boolean) => void,
): Promise<UnlistenFn> {
  return listen<boolean>("device:reachable", (e) => cb(e.payload));
}

export function onSyncProgress(
  cb: (event: ProgressEvent) => void,
): Promise<UnlistenFn> {
  return listen<ProgressEvent>("sync:progress", (e) => cb(e.payload));
}

export function onSyncPhase(
  cb: (phase: "pull" | "push") => void,
): Promise<UnlistenFn> {
  return listen<"pull" | "push">("sync:phase", (e) => cb(e.payload));
}

export function onKeyringWarning(
  cb: (message: string) => void,
): Promise<UnlistenFn> {
  return listen<string>("keyring:warning", (e) => cb(e.payload));
}

/// Fired when the SSH connect path accepted a first-seen host key
/// but couldn't persist it to the local known-hosts store —
/// typically a read-only / sandboxed config dir. The connection
/// works; subsequent reconnects just won't have a pinned fingerprint
/// to compare against. The app surfaces this as a toast so the
/// user knows the TOFU defence is degraded until they fix the
/// underlying FS / permissions issue.
export function onHostKeyWarning(
  cb: (message: string) => void,
): Promise<UnlistenFn> {
  return listen<string>("host-key:warning", (e) => cb(e.payload));
}

export function onLegacyFormatWarning(
  cb: (message: string) => void,
): Promise<UnlistenFn> {
  return listen<string>("document:legacy-format-warning", (e) =>
    cb(e.payload),
  );
}
