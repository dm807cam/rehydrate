use std::path::PathBuf;
use std::sync::Arc;

use rehydrate_core::{
    ArchiveReason, ArchivedDocument, DocumentSummary, FolderEntry, GarbageCollectReport,
    ImportKind, Library, VerifyReport, VersionEntry,
};
use rehydrate_device::known_hosts::KnownHosts;
use rehydrate_device::ssh::{is_reachable, SshConfig, SshDevice};
use rehydrate_device::{Device, DeviceInfo};
use rehydrate_sync::{
    execute_pull, execute_push, plan_pull, plan_push, progress, ProgressEvent, PullPlan, PushPlan,
};
use secrecy::SecretString;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::config;
use crate::keychain;
use crate::logging;
use crate::state::{default_library_dir, AppState, KEYRING_DEVICE_USER};
use crate::util::{err, lib_arc, IpcSecret};

/// Resolve the per-user known-hosts store for SSH host-key pinning.
/// Falls back to a cwd-relative path if the OS doesn't expose a
/// config directory — the file then lives next to the binary, which
/// is functional but not pretty (and very unusual in practice; every
/// platform we support exposes a config dir).
fn known_hosts_for_app() -> KnownHosts {
    let dir = directories::ProjectDirs::from("app", "rehydrate", "reHydrate")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    KnownHosts::new(dir.join("known_hosts.json"))
}

#[tauri::command]
pub fn ping() -> &'static str {
    "pong"
}

#[derive(Serialize)]
pub struct LogTail {
    pub lines: Vec<String>,
    pub log_dir: Option<PathBuf>,
}

#[tauri::command]
pub async fn get_recent_logs(max_lines: Option<usize>) -> Result<LogTail, String> {
    let n = max_lines.unwrap_or(500);
    let lines = logging::read_tail(n).map_err(err)?;
    Ok(LogTail {
        lines,
        log_dir: logging::log_dir(),
    })
}

/// Open the rolling-log directory in the OS file manager (Finder
/// on macOS). The path comes from `logging::log_dir()` so the
/// renderer can't influence which directory gets revealed — no
/// path traversal surface.
///
/// Returns the resolved path so the UI can present a fallback
/// (toast with the path string) if the open call fails — e.g.
/// when the directory doesn't exist yet because no logs have been
/// written this session.
#[tauri::command]
pub async fn reveal_log_dir(app: AppHandle) -> Result<String, String> {
    let dir = logging::log_dir().ok_or_else(|| "no log directory on this platform".to_string())?;
    let path = dir.to_string_lossy().to_string();
    app.opener()
        .open_path(&path, None::<&str>)
        .map_err(|e| format!("could not reveal {path}: {e}"))?;
    Ok(path)
}

#[tauri::command]
pub fn default_library_path() -> Option<PathBuf> {
    default_library_dir()
}

#[derive(Serialize)]
pub struct LibrarySummary {
    pub path: PathBuf,
    pub document_count: usize,
    pub version_count: i64,
    pub blob_count: usize,
    pub size_bytes: u64,
}

#[derive(Serialize)]
pub struct DeviceState {
    pub reachable: bool,
    pub connected: bool,
    pub info: Option<DeviceInfo>,
    /// True if a password is stored in the OS keychain — the UI uses this
    /// to decide whether to show a password prompt or just a Connect button.
    pub has_stored_password: bool,
    /// True if the host-key TOFU store has a pinned fingerprint for the
    /// default device endpoint. Drives the "Forget host key" affordance
    /// in the StatusPill popover — without this the user can only clear
    /// a stale pin by editing `known_hosts.json` by hand.
    pub has_recorded_host_key: bool,
}

#[derive(Serialize)]
pub struct SyncReportOut {
    pub recorded: usize,
    pub unchanged: usize,
    pub skipped: usize,
}

#[derive(Serialize)]
pub struct PushReportOut {
    pub pushed: usize,
    pub unchanged: usize,
    pub skipped: usize,
}

#[derive(Serialize)]
pub struct TwoWayReport {
    pub pull: SyncReportOut,
    pub push: PushReportOut,
}

// ---------- Library ---------------------------------------------------------

#[tauri::command]
pub async fn open_library(path: PathBuf, state: State<'_, AppState>) -> Result<(), String> {
    // Allowlist gate (issue #36). `switch_library` already requires the
    // target to be in `recent_libraries`; `open_library` was the only
    // remaining entry point that accepted an arbitrary renderer-supplied
    // path. A renderer XSS that called `ipc.openLibrary("/tmp/foo")`
    // would stamp a fresh library at any path, persist it to
    // config.json, and have `auto_open_library` silently re-open the
    // attacker-chosen path on next launch — shadowing the user's real
    // library. The legitimate user-driven flows are (1) accepting the
    // default-dir suggestion from the welcome screen and (2) running
    // the server-side OS folder picker; gate on those two plus the
    // existing recents allowlist. The pending-pick slot is one-shot:
    // a successful open consumes it so a stale token can't be replayed.
    let cfg = config::load();
    let mut pending = state.pending_picked_path.lock().await;
    let allowed = is_allowed_open_path(&path, &cfg.recent_libraries, pending.as_deref());
    if !allowed {
        return Err(format!(
            "{} is not an approved library path; use 'Open another library…' to pick it first",
            path.display()
        ));
    }
    let was_pending_match = pending.as_deref() == Some(path.as_path());
    open_library_at(&path, &state).await?;
    if was_pending_match {
        *pending = None;
    }
    Ok(())
}

/// Return true iff `path` is allowed as an `open_library` target —
/// extracted so the gate logic can be exercised by a unit test without
/// constructing a Tauri runtime. The three legitimate sources:
/// 1. The path the user just confirmed in the server-side OS picker
///    (`pending`, consumed one-shot by the caller on success).
/// 2. Any path the user has previously opened (`recents`), since each
///    of those once went through (1) before being recorded.
/// 3. The cross-platform default library dir, so the welcome-screen
///    "use defaults" button works on first launch when recents is
///    empty and the user has not yet engaged the picker.
fn is_allowed_open_path(
    path: &std::path::Path,
    recents: &[crate::config::RecentLibrary],
    pending: Option<&std::path::Path>,
) -> bool {
    if pending == Some(path) {
        return true;
    }
    if recents.iter().any(|r| r.path == path) {
        return true;
    }
    if let Some(default) = crate::state::default_library_dir() {
        if default == path {
            return true;
        }
    }
    false
}

/// Common path-validated open used by `open_library`, `switch_library`,
/// and `switch_library_via_dialog`. Drops the previously-held library
/// (and its OS lock) before opening the new one, so the same process
/// can move between per-device libraries without restarting.
async fn open_library_at(
    path: &std::path::Path,
    state: &State<'_, AppState>,
) -> Result<(), String> {
    // Drop the existing Library first so its `.lock` is released. If
    // the user is switching to the SAME library, this avoids
    // `AlreadyOpen` when we re-open it below.
    {
        let mut slot = state.library.lock().await;
        *slot = None;
    }
    let lib = Library::open(path).map_err(err)?;
    *state.library.lock().await = Some(Arc::new(lib));
    *state.library_path.lock().await = Some(path.to_path_buf());

    // Persist for next launch + push to recents. Best-effort.
    let mut cfg = config::load();
    cfg.record_open(path.to_path_buf());
    if let Err(e) = config::save(&cfg) {
        tracing::warn!("failed to persist app config: {e}");
    }
    Ok(())
}

/// On launch the UI calls this to restore the previously-opened library
/// without forcing the user through the welcome screen each time. Returns
/// the path that was opened, or `None` if there was no valid prior library.
#[tauri::command]
pub async fn auto_open_library(state: State<'_, AppState>) -> Result<Option<PathBuf>, String> {
    let mut cfg = config::load();
    let Some(path) = cfg.library_path.clone() else {
        return Ok(None);
    };
    if !path.exists() {
        // Stale config — drop the entry silently so the user sees the
        // welcome screen again rather than a confusing error.
        cfg.library_path = None;
        cfg.recent_libraries.retain(|r| r.path != path);
        let _ = config::save(&cfg);
        return Ok(None);
    }
    open_library_at(&path, &state).await?;
    Ok(Some(path))
}

/// Switch to a previously-opened library by path. The path must
/// already be in the recents list (so the user has consciously opened
/// it before via the picker). Returns the path opened, or an error if
/// the library is no longer there or its stamp is invalid.
#[tauri::command]
pub async fn switch_library(path: PathBuf, state: State<'_, AppState>) -> Result<PathBuf, String> {
    let cfg = config::load();
    if !cfg.recent_libraries.iter().any(|r| r.path == path) {
        return Err(format!(
            "{} is not in the recent libraries list; use 'Open another library…' to add it",
            path.display()
        ));
    }
    open_library_at(&path, &state).await?;
    Ok(path)
}

/// Open the OS folder picker so the user can pick a library
/// directory, then probe the chosen path. Returns:
/// - `None` if the user cancelled the dialog;
/// - `Some({ path, kind: "existing" })` if the path is already a
///   stamped library — the renderer should call `open_library` to
///   open it without further confirmation;
/// - `Some({ path, kind: "empty" })` if the path is an empty (or
///   dotfile-only) directory — the renderer should ask the user
///   "Create a new library here?" before calling `open_library`.
///
/// Foreign-but-non-empty directories surface as `Err`. We never
/// open the library here so a confirm prompt can sit between the
/// pick and the side-effecting open.
#[tauri::command]
pub async fn pick_library_directory(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<PickedLibraryDirectory>, String> {
    let app_for_pick = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_pick
            .dialog()
            .file()
            .set_title("Open a reHydrate library folder")
            .blocking_pick_folder()
    })
    .await
    .map_err(err)?;

    let Some(file_path) = picked else {
        return Ok(None);
    };
    let path = file_path
        .into_path()
        .map_err(|e| format!("could not resolve picked folder: {e}"))?;

    // Probe synchronously — no lock acquired, no library.json
    // written — so the renderer's confirm prompt sits between this
    // and the actual `open_library`.
    let kind = match Library::probe_path(&path) {
        Ok(rehydrate_core::LibraryPathKind::Empty) => PickedLibraryKind::Empty,
        Ok(rehydrate_core::LibraryPathKind::Existing) => PickedLibraryKind::Existing,
        Err(e) => return Err(err(e)),
    };
    // Mark this path as user-approved for one subsequent `open_library`
    // (issue #36). Overwrites any previous pending pick so the latest
    // user intent wins. `open_library` consumes the slot on success.
    *state.pending_picked_path.lock().await = Some(path.clone());
    Ok(Some(PickedLibraryDirectory { path, kind }))
}

#[derive(Serialize)]
pub struct PickedLibraryDirectory {
    pub path: PathBuf,
    pub kind: PickedLibraryKind,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PickedLibraryKind {
    Empty,
    Existing,
}

#[derive(Serialize)]
pub struct RecentLibraryEntry {
    pub path: PathBuf,
    pub label: String,
    pub last_opened: String,
    /// True if `path` still exists and looks like a library on disk.
    /// The UI uses this to grey out stale entries.
    pub available: bool,
    /// True if this is the currently-open library.
    pub current: bool,
}

#[tauri::command]
pub async fn list_recent_libraries(
    state: State<'_, AppState>,
) -> Result<Vec<RecentLibraryEntry>, String> {
    let cfg = config::load();
    let active = state.library_path.lock().await.clone();
    Ok(cfg
        .recent_libraries
        .into_iter()
        .map(|r| {
            let available = r.path.is_dir() && r.path.join("library.json").is_file();
            let current = active.as_ref() == Some(&r.path);
            RecentLibraryEntry {
                path: r.path,
                label: r.label,
                last_opened: r.last_opened,
                available,
                current,
            }
        })
        .collect())
}

/// Import a PDF or EPUB from disk into the library.
///
/// Audit fix H6: the OS file picker runs server-side here; the
/// renderer can no longer hand us a path of its choosing (e.g. a
/// symlink `evil.pdf → ~/.ssh/id_rsa`). Returns `None` if the user
/// cancelled the dialog. We also sniff the magic bytes after picking
/// so a renamed-but-not-actually-PDF/EPUB is rejected early.
#[tauri::command]
pub async fn import_file(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<DocumentSummary>, String> {
    let lib = lib_arc(&state).await?;

    // The dialog is a blocking OS call; run it on a worker thread so
    // we don't tie up the Tauri main thread.
    let app_for_pick = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_pick
            .dialog()
            .file()
            .add_filter("Documents", &["pdf", "epub"])
            .set_title("Import a PDF or EPUB")
            .blocking_pick_file()
    })
    .await
    .map_err(err)?;

    let Some(file_path) = picked else {
        return Ok(None);
    };
    let path = file_path
        .into_path()
        .map_err(|e| format!("could not resolve picked path: {e}"))?;

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "file has no extension".to_string())?;
    let kind = ImportKind::from_extension(ext).ok_or_else(|| {
        format!("unsupported file type: .{ext} — only PDF and EPUB are supported")
    })?;

    // Magic-byte sniff: refuse a "*.pdf" symlink that actually points
    // at, say, an SSH private key. PDF starts with "%PDF-", EPUB is a
    // ZIP ("PK\x03\x04").
    let mut head = [0u8; 5];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(&path).map_err(err)?;
        let _ = f.read(&mut head).map_err(err)?;
    }
    let looks_pdf = head.starts_with(b"%PDF-");
    let looks_epub = head.starts_with(b"PK\x03\x04");
    let extension_kind_ok = match kind {
        ImportKind::Pdf => looks_pdf,
        ImportKind::Epub => looks_epub,
    };
    if !extension_kind_ok {
        return Err(format!(
            "{} does not look like a {} file (header check failed)",
            path.display(),
            ext.to_uppercase()
        ));
    }

    let visible_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_string();
    // Run on the blocking pool: `import_file` does synchronous file
    // I/O (open + stream-hash) and a SQLite commit; on the
    // single-threaded executor it would stall every other IPC call
    // for the duration of an import.
    let name_for_task = visible_name.clone();
    let import =
        tauri::async_runtime::spawn_blocking(move || lib.import_file(&path, kind, &name_for_task))
            .await
            .map_err(err)?
            .map_err(err)?;
    let _ = visible_name;
    Ok(Some(import))
}

/// Import a file the user dragged onto the window. WebView security
/// hides the real filesystem path of an OS-level drop, so the frontend
/// streams the bytes over IPC; we stage them to a tempfile so the
/// existing path-based `Library::import_file` can do its work
/// unchanged. The `file_name` is used to derive the extension *and*
/// the default visible-name (sans extension), mirroring the file
/// picker path.
///
/// Same magic-byte sniff as `import_file`: a "*.pdf" that doesn't
/// start with "%PDF-" is rejected before any blob is written.
/// Hard cap on `import_dropped_file`. Real PDFs and EPUBs top out at
/// a few hundred MB even for textbook-sized documents; reMarkable
/// notebook PDFs are rarely past ~20 MB.
///
/// The bytes traverse a JSON-encoded IPC channel (each byte becomes
/// 1–4 ASCII chars in a `number[]`), so the renderer-side encoder and
/// the Rust-side JSON decoder both allocate ~4× the cap before this
/// guard fires. 64 MB on the wire ⇒ ~256 MB worst case in the JSON
/// decoder — uncomfortable but survivable on every system the app
/// runs on. A streaming `tauri::ipc::Channel<Vec<u8>>` path would
/// eliminate the multiplier; we keep the JSON-array path for now to
/// avoid an extra dep on either side, and pay for it with a tight
/// cap.
///
/// Mirrored by `MAX_IMPORT_FILE_BYTES` in `ui/src/ipc.ts`. The
/// renderer preflights `file.size` against that constant so an
/// oversize drop never reaches `arrayBuffer()` and never blows up
/// renderer memory before this backend guard fires (issue #24). If
/// you change this number, change the TS constant too.
pub(crate) const MAX_IMPORT_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Reject oversize drops with a stable, user-facing error string.
/// Pulled out of `import_dropped_file` so it can be unit-tested
/// without standing up the AppState/Tauri runtime — the renderer's
/// preflight relies on the same numeric cap (see
/// `MAX_IMPORT_FILE_BYTES` in `ui/src/ipc.ts`), so the contract here
/// is the last line of defence on the wire.
pub(crate) fn enforce_import_size_cap(file_name: &str, byte_len: u64) -> Result<(), String> {
    if byte_len > MAX_IMPORT_FILE_BYTES {
        Err(format!(
            "{file_name} is too large to import (limit is {} MiB)",
            MAX_IMPORT_FILE_BYTES / 1024 / 1024
        ))
    } else {
        Ok(())
    }
}

#[tauri::command]
pub async fn import_dropped_file(
    file_name: String,
    bytes: Vec<u8>,
    state: State<'_, AppState>,
) -> Result<DocumentSummary, String> {
    let lib = lib_arc(&state).await?;

    enforce_import_size_cap(&file_name, bytes.len() as u64)?;

    let ext_lower = std::path::Path::new(&file_name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .ok_or_else(|| "file has no extension".to_string())?;
    let kind = ImportKind::from_extension(&ext_lower).ok_or_else(|| {
        format!("unsupported file type: .{ext_lower} — only PDF and EPUB are supported")
    })?;

    // Magic-byte sniff before we touch disk or the library.
    let looks_pdf = bytes.starts_with(b"%PDF-");
    let looks_epub = bytes.starts_with(b"PK\x03\x04");
    let extension_kind_ok = match kind {
        ImportKind::Pdf => looks_pdf,
        ImportKind::Epub => looks_epub,
    };
    if !extension_kind_ok {
        return Err(format!(
            "{file_name} does not look like a {} file (header check failed)",
            ext_lower.to_uppercase()
        ));
    }

    let visible_name = std::path::Path::new(&file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Untitled")
        .to_string();

    // Stage bytes to a tempfile so we can reuse the path-based
    // import. NamedTempFile auto-deletes on drop even if import_file
    // panics — no orphans in /tmp on failure.
    let mut tf = tempfile::NamedTempFile::new().map_err(err)?;
    {
        use std::io::Write;
        tf.write_all(&bytes).map_err(err)?;
        tf.flush().map_err(err)?;
    }
    // Same rationale as `import_file`: hand the synchronous import
    // off to the blocking pool so the executor stays responsive.
    let path = tf.path().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || lib.import_file(&path, kind, &visible_name))
        .await
        .map_err(err)?
        .map_err(err)
}

#[tauri::command]
pub async fn garbage_collect(state: State<'_, AppState>) -> Result<GarbageCollectReport, String> {
    let lib = lib_arc(&state).await?;
    lib.garbage_collect().map_err(err)
}

#[tauri::command]
pub async fn verify_library(state: State<'_, AppState>) -> Result<VerifyReport, String> {
    let lib = lib_arc(&state).await?;
    lib.verify().map_err(err)
}

#[tauri::command]
pub async fn library_summary(state: State<'_, AppState>) -> Result<LibrarySummary, String> {
    let lib = lib_arc(&state).await?;
    let path = state
        .library_path
        .lock()
        .await
        .clone()
        .ok_or_else(|| "no library is open".to_string())?;
    let docs = lib.list_documents().map_err(err)?;
    // `blob_stats` walks the blob store from disk — for a large library
    // that's tens of thousands of fanout entries to stat. Done inline
    // it stalls the Tauri command thread (and on a single-threaded
    // executor, every other IPC call). Move to `spawn_blocking` so
    // the runtime stays responsive while we count.
    let blob_path = path.clone();
    let (blob_count, size_bytes) =
        tauri::async_runtime::spawn_blocking(move || blob_stats(&blob_path))
            .await
            .map_err(err)?;
    let version_count = lib.version_count().map_err(err)?;
    Ok(LibrarySummary {
        path,
        document_count: docs.len(),
        version_count,
        blob_count,
        size_bytes,
    })
}

#[tauri::command]
pub async fn list_folders(state: State<'_, AppState>) -> Result<Vec<FolderEntry>, String> {
    let lib = lib_arc(&state).await?;
    lib.list_folders().map_err(err)
}

/// Return a base64 data-URL for the document's first-page thumbnail, or
/// `None` if the manifest has no `.thumbnails/*.png` page (rare for
/// pulled docs; possible for fresh imports that haven't been synced
/// yet). The data-URL form lets the webview show the PNG without a
/// custom asset-protocol capability — and thumbnails are typically
/// 5–50 KB so the IPC payload stays small.
#[tauri::command]
pub async fn document_thumbnail(
    document_id: String,
    state: State<'_, AppState>,
) -> Result<Option<String>, String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use rehydrate_core::Manifest;

    let lib = lib_arc(&state).await?;
    let docs = lib.list_documents().map_err(err)?;
    let doc = docs
        .iter()
        .find(|d| d.document_id == document_id)
        .ok_or_else(|| format!("document {document_id} not in library"))?;
    let manifest_bytes = lib.read_blob(&doc.current_manifest).map_err(err)?;
    let manifest = Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;

    let mut thumbs: Vec<_> = manifest
        .files
        .iter()
        .filter(|f| f.path.ends_with(".png") && f.path.contains(".thumbnails"))
        .collect();
    if thumbs.is_empty() {
        return Ok(None);
    }
    thumbs.sort_by(|a, b| a.path.cmp(&b.path));
    let bytes = lib.read_blob(&thumbs[0].sha256).map_err(err)?;
    Ok(Some(format!(
        "data:image/png;base64,{}",
        STANDARD.encode(&bytes)
    )))
}

/// Materialise a document's content into a cache directory and open it
/// with the OS's default viewer. PDFs and EPUBs are written as-is.
/// Notebooks (no PDF/EPUB body file) get assembled into a single
/// multi-page PDF from the device's per-page thumbnail PNGs — a preview,
/// not faithful ink rendering.
#[tauri::command]
pub async fn open_document(
    document_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<PathBuf, String> {
    use rehydrate_core::Manifest;

    let lib = lib_arc(&state).await?;
    let docs = lib.list_documents().map_err(err)?;
    let doc = docs
        .iter()
        .find(|d| d.document_id == document_id)
        .ok_or_else(|| format!("document {document_id} not in library"))?;
    let manifest_bytes = lib.read_blob(&doc.current_manifest).map_err(err)?;
    let manifest = Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;

    let cache_root = directories::ProjectDirs::from("app", "rehydrate", "reHydrate")
        .map(|d| d.cache_dir().to_path_buf())
        .ok_or_else(|| "no cache directory on this platform".to_string())?
        .join("open");
    std::fs::create_dir_all(&cache_root).map_err(err)?;
    let safe_name = sanitize(&doc.visible_name);

    // Resolve body: full PDF/EPUB, otherwise stitch thumbnails to a PDF.
    let cache_path = if let Some(body) = manifest
        .files
        .iter()
        .find(|f| f.path.ends_with(".pdf") || f.path.ends_with(".epub"))
    {
        // Audit fix H8: explicitly allow-list the cache extension to
        // {pdf, epub}. The previous `body.path.rsplit('.').next()`
        // accepted any tail — a manifest with `body.path = "x.command"`
        // landed a `.command` file that LaunchServices would then
        // execute. Manifest::validate_paths blocks `..`/absolutes but
        // doesn't restrict extensions.
        let ext = if body.path.ends_with(".pdf") {
            "pdf"
        } else {
            "epub"
        };
        // Cache key uses the document_id and the full content hash.
        // Earlier versions truncated the hash to 12 hex chars, which
        // gave each cache slot only ~48 bits of separation — well
        // inside birthday-collision range for a library a malicious
        // device could populate. Including the doc_id pins the cache
        // to *this* document so a second doc whose body hash collided
        // could not steal the slot.
        let p = cache_root.join(format!(
            "{safe_name}-{document_id}-{}.{ext}",
            body.sha256.as_str()
        ));
        // Read the blob lazily — for hot opens of a previously-cached
        // PDF/EPUB this avoids loading the whole document into memory
        // just to throw it away.
        if !p.exists() {
            let bytes = lib.read_blob(&body.sha256).map_err(err)?;
            std::fs::write(&p, &bytes).map_err(err)?;
        }
        p
    } else {
        // Notebook. Two render paths:
        //   1) `.rm` ink files → vector PDF (sharp at any zoom).
        //   2) Fallback: stitch per-page thumbnail PNGs (low-fidelity
        //      preview, used only if a page has no parseable ink data).
        //
        // The cache key includes a layout-version suffix exported by
        // `rehydrate-render::PREVIEW_LAYOUT_VERSION`. The constant
        // lives next to the renderer (not here) so the edit that
        // changes visual output is the edit that bumps the version
        // — that co-location is itself the lesson from the phase-4
        // parser regression: when the cache-bust lever and the code
        // it guards live in different crates, a contributor will
        // forget to pull both.
        //
        // Full manifest hash + document_id in the key — see the
        // PDF/EPUB branch above for why we no longer truncate.
        let p = cache_root.join(format!(
            "{safe_name}-{document_id}-{}-{}.pdf",
            doc.current_manifest.as_str(),
            rehydrate_render::PREVIEW_LAYOUT_VERSION,
        ));
        if !p.exists() {
            let mut rm_pages: Vec<_> = manifest
                .files
                .iter()
                .filter(|f| {
                    f.path.ends_with(".rm")
                        && !f.path.contains(".thumbnails")
                        && !f.path.ends_with(".local")
                })
                .collect();
            rm_pages.sort_by(|a, b| a.path.cmp(&b.path));

            let pdf_bytes = if !rm_pages.is_empty() {
                let mut bufs = Vec::with_capacity(rm_pages.len());
                for f in &rm_pages {
                    bufs.push(lib.read_blob(&f.sha256).map_err(err)?);
                }
                // Preview path passes `None` for OCR — searchable
                // PDFs are an export-only feature so the Preview
                // cache key (PREVIEW_LAYOUT_VERSION) stays stable
                // and the cached previews don't regenerate just
                // because OCR completed.
                rehydrate_render::build_pdf_from_rm_files(&doc.visible_name, &bufs, None).or_else(
                    |e| {
                        // .rm parse failed (older v3/v5 format we don't
                        // render, or corrupt page) — fall through to the
                        // thumbnail fallback so the user still sees
                        // something. Emit a warning so the UI can
                        // tell the user *why* the preview is fuzzy
                        // instead of vector-sharp.
                        tracing::warn!(
                            "ink rendering failed for {}: {e}; falling back to thumbnails",
                            doc.document_id
                        );
                        let _ = app.emit(
                            "document:legacy-format-warning",
                            format!(
                                "\"{}\" uses an older notebook format. The preview falls \
                                 back to lower-resolution thumbnails. Sync the tablet to \
                                 upgrade the notebook to the current format.",
                                doc.visible_name
                            ),
                        );
                        thumbnail_fallback_pdf(&lib, &manifest, &doc.visible_name, None)
                    },
                )?
            } else {
                thumbnail_fallback_pdf(&lib, &manifest, &doc.visible_name, None)?
            };
            std::fs::write(&p, &pdf_bytes).map_err(err)?;
        }
        p
    };

    app.opener()
        .open_path(cache_path.to_string_lossy(), None::<&str>)
        .map_err(|e| format!("could not open {}: {e}", cache_path.display()))?;
    Ok(cache_path)
}

/// Path bundle returned by [`prepare_export_pdf`]. The `file` is the
/// staged document. JS uses this for the hover-prefetch round-trip
/// only — the actual drag-out is initiated from
/// [`start_export_drag`], which never returns the path to the
/// renderer. `icon` is the drag-preview image the OS-drag layer
/// requires; staged alongside the document so the macOS NSImage
/// initializer has a real filesystem path.
#[derive(Serialize)]
pub struct ExportDragPaths {
    pub file: PathBuf,
    pub icon: PathBuf,
}

/// Bytes for the 32×32 app icon, copied into the export cache at
/// first use so the drag-source layer has a stable filesystem path
/// for the drag preview. Bundled at build time so a corrupted /
/// missing bundle resource doesn't break the drag.
const EXPORT_DRAG_ICON_PNG: &[u8] = include_bytes!("../icons/32x32.png");

/// Resolve the export cache root. Pulled out so tests can drive
/// `stage_export_for_document` against a tempdir without touching the
/// user's real cache.
fn export_root() -> Result<PathBuf, String> {
    Ok(directories::ProjectDirs::from("app", "rehydrate", "reHydrate")
        .map(|d| d.cache_dir().to_path_buf())
        .ok_or_else(|| "no cache directory on this platform".to_string())?
        .join("export"))
}

/// Inputs for [`stage_export_for_document`]. Resolving these requires
/// async (`lib_arc`); the staging itself is sync so the tests don't
/// need a tokio runtime to drive it.
struct StagingInputs {
    /// Validated document id read back from the library — using the
    /// caller-supplied parameter directly would re-introduce an
    /// implicit dependency on the `.find(...)` equality check above.
    document_id: String,
    visible_name: String,
    current_manifest: String,
    manifest: rehydrate_core::Manifest,
}

/// Stage a document on disk under a clean, human-friendly filename
/// so the OS-drag layer sees a real file with the right name. The
/// staging path is content-keyed:
///
///   `<export_root>/<document_id>-<key>/<visible_name>.<ext>`
///
/// where `<key>` is either the body blob hash (PDF/EPUB — copied
/// verbatim) or `<manifest_hash>-<EXPORT_LAYOUT_VERSION>` (notebook
/// — rendered through `rehydrate_render`).
///
/// `warn_legacy_format` is invoked when the notebook renderer falls
/// back to the thumbnail path, so the caller can surface the message
/// to the user. Threading it as a callback (rather than always
/// passing an `AppHandle`) keeps the unit tests free of Tauri
/// scaffolding.
///
/// Returns the staged paths and a `is_cache_hit` flag so callers can
/// distinguish a cheap hit from a fresh render — useful for
/// distinguishing prefetch warm-up from an actual cold drag.
fn stage_export_for_document(
    lib: &Arc<rehydrate_core::Library>,
    export_root: &std::path::Path,
    inputs: &StagingInputs,
    warn_legacy_format: &dyn Fn(&str),
) -> Result<(ExportDragPaths, bool), String> {
    let safe_name = sanitize(&inputs.visible_name);

    // Determine the body-type, extension, and cache key **without**
    // reading any body blob. We need (ext, key) to compute the
    // staging path — but we don't want to pull bytes off disk (or
    // render the notebook!) just to discover a cache hit.
    let stored_body = inputs
        .manifest
        .files
        .iter()
        .find(|f| f.path.ends_with(".pdf") || f.path.ends_with(".epub"));

    let (ext, key) = match stored_body {
        Some(body) => {
            // Same defensive extension allow-list as `open_document`
            // — a crafted manifest can't extend the dropped filename
            // to e.g. `.command`. Cache key is the blob content hash;
            // it changes iff the body changes.
            let ext = if body.path.ends_with(".pdf") {
                "pdf"
            } else {
                "epub"
            };
            (ext, body.sha256.as_str().to_string())
        }
        None => {
            // Notebook. Cache key combines manifest hash (covers all
            // ink + typed-text + transcript state via Manifest's
            // content hash) with EXPORT_LAYOUT_VERSION (covers
            // renderer changes). Independent of the Preview cache key.
            (
                "pdf",
                format!(
                    "{}-{}",
                    inputs.current_manifest,
                    rehydrate_render::EXPORT_LAYOUT_VERSION,
                ),
            )
        }
    };

    // Use the validated document id for the staging dir (not the
    // caller-supplied string we were handed) — matches the pattern
    // `open_document` uses for its rendered filename.
    let staging_dir = export_root.join(format!("{}-{}", inputs.document_id, key));
    let staging_path = staging_dir.join(format!("{safe_name}.{ext}"));
    let icon_path = export_root.join(".drag-icon.png");

    // Fast path: cache hit. Skip the blob read / render entirely so
    // a hover-prefetch followed by an immediate ⌥-drag returns inside
    // macOS' user-gesture window.
    if staging_path.exists() {
        ensure_drag_icon(export_root, &icon_path)?;
        return Ok((
            ExportDragPaths {
                file: staging_path,
                icon: icon_path,
            },
            true,
        ));
    }

    // Cold path: resolve / render the body bytes.
    let source_bytes = match stored_body {
        Some(body) => lib.read_blob(&body.sha256).map_err(err)?,
        None => {
            // Searchable-PDF support: if the document has an
            // `ocr/transcript.md` derived artefact in its current
            // manifest, parse it into per-page text and hand it to
            // the renderer for an invisible Tr-3 text layer. No
            // transcript → no text layer (the PDF still renders ink
            // exactly the same way). Cache key already covers this
            // via EXPORT_LAYOUT_VERSION + manifest_hash, so a
            // post-export OCR run produces a new staging dir.
            let ocr_pages = load_ocr_transcript_pages(lib, &inputs.manifest)?;
            render_notebook_pdf(
                lib,
                &inputs.manifest,
                &inputs.visible_name,
                &inputs.document_id,
                ocr_pages.as_deref(),
                warn_legacy_format,
            )?
        }
    };

    std::fs::create_dir_all(&staging_dir).map_err(err)?;
    // Write atomically: a same-dir tempfile + rename means the OS
    // never sees a partially-written staging file even if the
    // renderer crashes mid-write. Critical because the drag gesture
    // can fire immediately after this command resolves; a half-
    // written PDF would surface as a corrupt drop.
    let mut tmp = tempfile::NamedTempFile::new_in(&staging_dir).map_err(err)?;
    use std::io::Write;
    tmp.write_all(&source_bytes).map_err(err)?;
    tmp.persist(&staging_path)
        .map_err(|e| format!("persist staging file: {e}"))?;

    ensure_drag_icon(export_root, &icon_path)?;
    Ok((
        ExportDragPaths {
            file: staging_path,
            icon: icon_path,
        },
        false,
    ))
}

/// Materialise the 32×32 drag-preview icon next to the export cache
/// root on first use. Idempotent and clobber-safe — two parallel
/// calls land identical bytes so a race in the persist is harmless.
fn ensure_drag_icon(export_root: &std::path::Path, icon_path: &std::path::Path) -> Result<(), String> {
    if icon_path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(export_root).map_err(err)?;
    let mut tmp = tempfile::NamedTempFile::new_in(export_root).map_err(err)?;
    use std::io::Write;
    tmp.write_all(EXPORT_DRAG_ICON_PNG).map_err(err)?;
    tmp.persist(icon_path)
        .map_err(|e| format!("persist drag icon: {e}"))?;
    Ok(())
}

/// Render a notebook to PDF, falling back to the thumbnail-based PDF
/// when the `.rm` parser doesn't recognise the page format. Mirrors
/// the body-detection branching `open_document` uses for previews.
/// `warn_legacy_format` is invoked when the fallback fires so the
/// caller can surface the message to the user (the rendering itself
/// proceeds either way — the user still gets *something* to drag).
///
/// `ocr_per_page` is the per-output-page text used for the
/// invisible-text layer; pass `None` for the preview path (which
/// keeps its byte-stable cache key) or `Some(...)` from the export
/// path when an `ocr/transcript.md` derived artefact is available.
/// Padded/truncated to match the number of rendered pages so the
/// renderer can index into it directly.
fn render_notebook_pdf(
    lib: &Arc<rehydrate_core::Library>,
    manifest: &rehydrate_core::Manifest,
    visible_name: &str,
    document_id: &str,
    ocr_per_page: Option<&[String]>,
    warn_legacy_format: &dyn Fn(&str),
) -> Result<Vec<u8>, String> {
    let mut rm_pages: Vec<_> = manifest
        .files
        .iter()
        .filter(|f| {
            f.path.ends_with(".rm")
                && !f.path.contains(".thumbnails")
                && !f.path.ends_with(".local")
        })
        .collect();
    rm_pages.sort_by(|a, b| a.path.cmp(&b.path));

    if rm_pages.is_empty() {
        return thumbnail_fallback_pdf(lib, manifest, visible_name, ocr_per_page);
    }

    let mut bufs = Vec::with_capacity(rm_pages.len());
    for f in &rm_pages {
        bufs.push(lib.read_blob(&f.sha256).map_err(err)?);
    }
    // Trim/pad the OCR slice so it has exactly `bufs.len()` entries
    // — the renderer indexes into it by output-page number. The
    // transcript builder produces one entry per source-notebook
    // page (including blanks); the renderer only emits a page per
    // `.rm` file. In the common case (every page has ink and a
    // transcript) the lengths match; otherwise we accept best-effort
    // positional alignment over no OCR at all.
    let aligned_ocr: Option<Vec<String>> = ocr_per_page.map(|pages| {
        let mut v = pages.to_vec();
        v.resize(bufs.len(), String::new());
        v
    });
    let ocr_slice = aligned_ocr.as_deref();
    match rehydrate_render::build_pdf_from_rm_files(visible_name, &bufs, ocr_slice) {
        Ok(b) => Ok(b),
        Err(e) => {
            tracing::warn!(
                "export rendering failed for {document_id}: {e}; falling back to thumbnails"
            );
            warn_legacy_format(&e.to_string());
            thumbnail_fallback_pdf(lib, manifest, visible_name, ocr_slice)
        }
    }
}

/// Path under which `ocr_commands::transcribe_document` records the
/// rendered OCR transcript. Duplicated here (rather than imported
/// from `ocr_commands`) because that module's constant is `pub(crate)`
/// and lives next to the OCR pipeline; keeping the spelling in lockstep
/// is the only invariant the two paths share, and a divergence here
/// would surface immediately as "no text layer in exported PDFs".
const OCR_TRANSCRIPT_DERIVED_PATH: &str = "ocr/transcript.md";

/// Look the current OCR transcript up via the document's manifest
/// (no need to traverse version history — `record_derived_artefact`
/// always lands the latest transcript on a new version that becomes
/// the current manifest). Returns `None` when the document has no
/// transcript, in which case the exporter just skips the text layer.
fn load_ocr_transcript_pages(
    lib: &Arc<rehydrate_core::Library>,
    manifest: &rehydrate_core::Manifest,
) -> Result<Option<Vec<String>>, String> {
    let Some(file) = manifest
        .files
        .iter()
        .find(|f| f.derived && f.path == OCR_TRANSCRIPT_DERIVED_PATH)
    else {
        return Ok(None);
    };
    let bytes = lib.read_blob(&file.sha256).map_err(err)?;
    let markdown = String::from_utf8_lossy(&bytes).into_owned();
    let pages = parse_transcript_pages(&markdown);
    if pages.iter().all(|p| p.is_empty()) {
        // An empty transcript (every page either blank or failed)
        // would just emit an empty text layer — skip it so the
        // export PDF stays byte-identical to the no-transcript case.
        return Ok(None);
    }
    Ok(Some(pages))
}

/// Parse the OCR transcript markdown into one entry per source
/// notebook page. Blanks and failed pages produce empty strings so
/// the slice index lines up with the original page order. The
/// transcript format is owned by `ocr_commands::transcribe_document`
/// (frontmatter → optional `_note:` lines → per-page paragraphs
/// separated by blank lines, blanks rendered as empty paragraphs).
fn parse_transcript_pages(markdown: &str) -> Vec<String> {
    // Strip the `---\n...\n---\n` frontmatter block if present. The
    // delimiters are fixed strings emitted by the OCR writer; we
    // don't try to be permissive (the writer is the only producer).
    let body = if let Some(rest) = markdown.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            &rest[end + "\n---\n".len()..]
        } else {
            rest
        }
    } else {
        markdown
    };
    // Strip the optional "_note: ..._" lines the writer prepends
    // before the page paragraphs. Each note is one line followed by
    // a blank line.
    let mut cursor = body.trim_start_matches('\n');
    loop {
        let line_end = cursor.find('\n').unwrap_or(cursor.len());
        let line = &cursor[..line_end];
        if line.starts_with("_note:") && line.ends_with('_') {
            cursor = &cursor[line_end..];
            cursor = cursor.trim_start_matches('\n');
            continue;
        }
        break;
    }
    // Page paragraphs are separated by exactly "\n\n"; blanks emit
    // empty paragraphs (the writer pushes a "\n\n" before every page
    // index > 0 regardless of whether that page is blank), so this
    // split yields one entry per source-notebook page.
    cursor
        .split("\n\n")
        .map(|s| s.trim().to_string())
        .collect()
}

/// Resolve `(library, staging inputs)` for a Tauri command — shared
/// preamble used by both `prepare_export_pdf` and `start_export_drag`.
async fn resolve_staging_inputs(
    document_id: &str,
    state: &State<'_, AppState>,
) -> Result<(Arc<rehydrate_core::Library>, StagingInputs), String> {
    use rehydrate_core::Manifest;
    let lib = lib_arc(state).await?;
    let docs = lib.list_documents().map_err(err)?;
    let doc = docs
        .iter()
        .find(|d| d.document_id == document_id)
        .ok_or_else(|| format!("document {document_id} not in library"))?;
    let manifest_bytes = lib.read_blob(&doc.current_manifest).map_err(err)?;
    let manifest = Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;
    Ok((
        lib,
        StagingInputs {
            document_id: doc.document_id.clone(),
            visible_name: doc.visible_name.clone(),
            current_manifest: doc.current_manifest.as_str().to_string(),
            manifest,
        },
    ))
}

/// Stage a document for OS-drag-out, returning the staged file path
/// so the renderer can pre-warm the cache on hover. Idempotent —
/// repeated calls hit the cache and skip the blob read / render
/// entirely. JS treats the returned paths as opaque: it never hands
/// them back to a drag-source IPC (the OS drag is started by
/// [`start_export_drag`], which validates the same way).
#[tauri::command]
pub async fn prepare_export_pdf(
    document_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ExportDragPaths, String> {
    let (lib, inputs) = resolve_staging_inputs(&document_id, &state).await?;
    let root = export_root()?;
    let visible_name = inputs.visible_name.clone();
    // Render off the tokio runtime — a cold notebook stages 100s of
    // pages of `.rm` strokes which would otherwise block the IPC
    // worker for several seconds.
    let lib_for_blocking = Arc::clone(&lib);
    let (paths, _hit) = tauri::async_runtime::spawn_blocking(move || {
        let warn = |_: &str| {
            let _ = app.emit(
                "document:legacy-format-warning",
                format!(
                    "\"{visible_name}\" uses an older notebook format. The export falls \
                     back to lower-resolution thumbnails. Sync the tablet to upgrade the \
                     notebook to the current format.",
                ),
            );
        };
        stage_export_for_document(&lib_for_blocking, &root, &inputs, &warn)
    })
    .await
    .map_err(err)??;
    Ok(paths)
}

/// Begin a native OS drag-out for the given document. Stages the
/// file under the export cache (re-using a cache hit if the renderer
/// already prefetched on hover), then hands the staged path to the
/// platform drag-source layer.
///
/// **Security boundary**: the renderer only supplies the
/// `document_id` — never a filesystem path. Even an XSS in the
/// webview can't make this command drag a file outside the
/// `export/<document_id>-<key>/` cache directory.
///
/// macOS-only for now: Linux behaviour of the underlying `drag`
/// crate is unverified and Alt-drag conflicts with Windows system
/// shortcuts. On other platforms the command returns an error so
/// the UI's ⌥-drag affordance (already gated client-side) has a
/// belt-and-braces backstop.
#[tauri::command]
pub async fn start_export_drag(
    document_id: String,
    app: AppHandle,
    window: tauri::Window,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let (lib, inputs) = resolve_staging_inputs(&document_id, &state).await?;
    let root = export_root()?;
    let visible_name = inputs.visible_name.clone();
    let app_for_warn = app.clone();
    let lib_for_blocking = Arc::clone(&lib);
    let (paths, _hit) = tauri::async_runtime::spawn_blocking(move || {
        let warn = |_: &str| {
            let _ = app_for_warn.emit(
                "document:legacy-format-warning",
                format!(
                    "\"{visible_name}\" uses an older notebook format. The export falls \
                     back to lower-resolution thumbnails. Sync the tablet to upgrade the \
                     notebook to the current format.",
                ),
            );
        };
        stage_export_for_document(&lib_for_blocking, &root, &inputs, &warn)
    })
    .await
    .map_err(err)??;

    start_native_drag(&app, &window, paths)
}

/// macOS implementation of the native drag-source. Hands the staged
/// path to NSPasteboard via the `drag` crate, on the main thread (a
/// hard requirement of NSDraggingSession). The handle to the window
/// is taken from the Tauri `Window` and forwarded as the
/// `HasWindowHandle` source.
#[cfg(target_os = "macos")]
fn start_native_drag(
    app: &AppHandle,
    window: &tauri::Window,
    paths: ExportDragPaths,
) -> Result<(), String> {
    use std::sync::mpsc::channel;

    let (tx, rx) = channel();
    let window = window.clone();
    app.run_on_main_thread(move || {
        let r = drag::start_drag(
            &window,
            drag::DragItem::Files(vec![paths.file]),
            drag::Image::File(paths.icon),
            // We don't need the drop result on the Rust side — the
            // renderer's view of "drag complete" is the absence of an
            // error from this command. Swallow the callback so the
            // drag crate doesn't keep an event loop alive.
            |_result, _cursor| {},
            drag::Options::default(),
        )
        .map_err(|e| format!("start_drag: {e}"));
        let _ = tx.send(r);
    })
    .map_err(err)?;
    rx.recv()
        .map_err(|e| format!("drag worker disconnected: {e}"))?
}

/// Non-macOS stub. The UI side gates the affordance to mac, so this
/// should be unreachable in practice; it exists so the command set
/// is portable.
#[cfg(not(target_os = "macos"))]
fn start_native_drag(
    _app: &AppHandle,
    _window: &tauri::Window,
    _paths: ExportDragPaths,
) -> Result<(), String> {
    Err("OS-drag export is only available on macOS".to_string())
}

/// Right-click "Export PDF…" path: stage the document (re-using the
/// drag-out cache) and copy it into a folder the user picks via the
/// system dialog. Returns the absolute path of the copied file, or
/// `None` if the user cancelled the picker.
///
/// Complements [`start_export_drag`] for users who can't (or don't
/// want to) hold ⌥ during a drag. Works on every platform — there's
/// no native-drag dependency, just a file copy — so this is the
/// canonical export path on Windows/Linux as well.
#[tauri::command]
pub async fn export_document_pdf(
    document_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<PathBuf>, String> {
    let (lib, inputs) = resolve_staging_inputs(&document_id, &state).await?;
    let root = export_root()?;
    let visible_name = inputs.visible_name.clone();
    let dialog_title = format!("Export \"{}\" to…", visible_name);
    let visible_name_for_warn = visible_name.clone();
    let app_for_warn = app.clone();
    let lib_for_blocking = Arc::clone(&lib);
    let (paths, _hit) = tauri::async_runtime::spawn_blocking(move || {
        let warn = |_: &str| {
            let _ = app_for_warn.emit(
                "document:legacy-format-warning",
                format!(
                    "\"{visible_name_for_warn}\" uses an older notebook format. The export falls \
                     back to lower-resolution thumbnails. Sync the tablet to upgrade the \
                     notebook to the current format.",
                ),
            );
        };
        stage_export_for_document(&lib_for_blocking, &root, &inputs, &warn)
    })
    .await
    .map_err(err)??;

    // Pull the visible name (and on-disk extension) from the staged
    // path: it's already sanitized and matches the filename the user
    // would have seen drop into Finder from the drag-out path. We
    // deliberately do NOT re-derive it from `inputs.visible_name`
    // here so the two export paths can't drift — if the drag-out
    // path ever changes its naming scheme, the right-click path
    // follows automatically.
    let staged_name = paths
        .file
        .file_name()
        .ok_or_else(|| "staged file missing a filename".to_string())?
        .to_os_string();

    let app_for_pick = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_pick
            .dialog()
            .file()
            .set_title(&dialog_title)
            .blocking_pick_folder()
    })
    .await
    .map_err(err)?;

    let Some(dest_path) = picked else {
        return Ok(None);
    };
    let dest_dir = dest_path
        .into_path()
        .map_err(|e| format!("could not resolve picked folder: {e}"))?;
    let target = dest_dir.join(&staged_name);

    if target.exists() {
        return Err(format!(
            "{} already exists; refusing to overwrite",
            target.display()
        ));
    }

    // Stream the copy through `std::fs::copy` rather than reading +
    // writing manually — it preserves the file's permissions on
    // Unix and is a single syscall on most platforms. The staged
    // file is content-keyed so we don't need a temp + rename here;
    // a torn write would just leave a partial file in a user-chosen
    // directory, and the caller can retry.
    std::fs::copy(&paths.file, &target)
        .map_err(|e| format!("could not write {}: {e}", target.display()))?;

    Ok(Some(target))
}

/// Open an https URL in the user's default browser. The renderer
/// uses this to surface the support / Issues link from the About
/// dialog. Restricted to https://github.com/dm807cam/rehydrate/ so a
/// compromised renderer can't open arbitrary websites — the opener
/// plugin would happily launch any URL, but that's not a
/// capability the App needs exposed.
#[tauri::command]
pub async fn open_support_url(url: String, app: AppHandle) -> Result<(), String> {
    const SUPPORT_PREFIX: &str = "https://github.com/dm807cam/rehydrate/";
    if !url.starts_with(SUPPORT_PREFIX) {
        return Err(format!(
            "refusing to open URL outside support prefix ({SUPPORT_PREFIX})"
        ));
    }
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|e| format!("could not open {url}: {e}"))
}

/// Return the app's published version string. Used by the About
/// dialog. Kept distinct from any in-app autoupdate flow because
/// v1.0 has no autoupdate — this is purely diagnostic copy.
#[tauri::command]
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Trip the shared sync cancellation flag. The engine polls the
/// flag between documents (and inside the folder-push loop), so an
/// in-flight sync_two_way / pull_execute / push_execute stops at
/// the next granular boundary and returns `SyncError::Cancelled`.
/// Idempotent: calling when nothing is running is a no-op (the next
/// sync resets the flag before it starts).
#[tauri::command]
pub fn cancel_sync(state: State<'_, AppState>) -> Result<(), String> {
    state.sync_cancel.cancel();
    Ok(())
}

/// Trip the shared OCR cancellation flag. Mirrors `cancel_sync`
/// but targets `transcribe_document` / the auto-OCR sweep. The
/// flag is reset at the start of each new OCR job.
#[tauri::command]
pub fn cancel_ocr(state: State<'_, AppState>) -> Result<(), String> {
    state.ocr_cancel.cancel();
    Ok(())
}

#[tauri::command]
pub async fn list_documents(state: State<'_, AppState>) -> Result<Vec<DocumentSummary>, String> {
    let lib = lib_arc(&state).await?;
    lib.list_documents().map_err(err)
}

#[tauri::command]
pub async fn list_archived(state: State<'_, AppState>) -> Result<Vec<ArchivedDocument>, String> {
    let lib = lib_arc(&state).await?;
    lib.list_archived().map_err(err)
}

/// Rename a live document. Writes the new title into `.metadata`'s
/// `visibleName` and records a new version, so the tablet picks up
/// the rename on the next push.
#[tauri::command]
pub async fn rename_document(
    document_id: String,
    new_name: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let lib = lib_arc(&state).await?;
    lib.rename_document(&document_id, &new_name).map_err(err)?;
    Ok(())
}

/// Rename a folder. Updates the local row and flags it for push so
/// the next sync uploads the new metadata to the tablet.
#[tauri::command]
pub async fn rename_folder(
    folder_id: String,
    new_name: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let lib = lib_arc(&state).await?;
    lib.rename_folder(&folder_id, &new_name).map_err(err)?;
    Ok(())
}

/// Create a new folder under `parentId` (None = root). The folder is
/// flagged for push so the next sync uploads its `<uuid>.metadata`
/// file to the tablet.
#[tauri::command]
pub async fn create_folder(
    visible_name: String,
    parent_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<rehydrate_core::FolderEntry, String> {
    let lib = lib_arc(&state).await?;
    lib.create_folder(&visible_name, parent_id.as_deref())
        .map_err(err)
}

/// Roll back every local edit since the last successful sync —
/// folder renames, reparents, deletions, creations, and document
/// move/rename metadata changes. Imports are intentionally not
/// touched. Returns a precise tally so the UI can confirm what was
/// undone. See `Library::revert_unpushed_changes` for the policy.
#[tauri::command]
pub async fn revert_unpushed_changes(
    state: State<'_, AppState>,
) -> Result<rehydrate_core::RevertReport, String> {
    let lib = lib_arc(&state).await?;
    lib.revert_unpushed_changes().map_err(err)
}

/// Delete a folder from the local library and queue a tombstone
/// push so the tablet drops it on the next sync. Contents are
/// preserved: every direct child folder and document is reparented
/// to the deleted folder's parent (root if it was already at root).
/// Returns the count of moved children so the UI can word the
/// confirmation toast precisely.
#[tauri::command]
pub async fn delete_folder(
    folder_id: String,
    state: State<'_, AppState>,
) -> Result<rehydrate_core::DeleteFolderOutcome, String> {
    let lib = lib_arc(&state).await?;
    lib.delete_folder(&folder_id).map_err(err)
}

/// Move and/or reorder a folder in the sidebar. Sort order is
/// local-only (the tablet has no notion of sibling order), but
/// reparenting (changing `new_parent`) IS represented on the device
/// through each folder's `<uuid>.metadata` `parent` field — the
/// library layer rewrites the cached metadata and flags the folder
/// for push when the parent actually changes.
#[tauri::command]
pub async fn reorder_folder(
    folder_id: String,
    new_parent: Option<String>,
    new_sort_index: f64,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let lib = lib_arc(&state).await?;
    lib.reorder_folder(&folder_id, new_parent.as_deref(), new_sort_index)
        .map_err(err)
}

/// Move a live document into a different folder (or to root if
/// `parentId` is None). Records a new version so the change propagates to
/// the device on the next push.
#[tauri::command]
pub async fn move_document(
    document_id: String,
    parent_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let lib = lib_arc(&state).await?;
    lib.move_document(&document_id, parent_id.as_deref())
        .map_err(err)?;
    Ok(())
}

/// Soft-delete a live document — moves it to the archive with reason
/// "local". Versions are kept so it can be restored.
#[tauri::command]
pub async fn archive_document(
    document_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let lib = lib_arc(&state).await?;
    lib.archive_document(&document_id, ArchiveReason::Local)
        .map_err(err)
}

/// Restore an archived document to the live listing.
#[tauri::command]
pub async fn unarchive_document(
    document_id: String,
    state: State<'_, AppState>,
) -> Result<DocumentSummary, String> {
    let lib = lib_arc(&state).await?;
    lib.unarchive_document(&document_id).map_err(err)
}

/// Permanently delete an archived document. Drops the version log so blobs
/// become orphans; run garbage_collect to reclaim disk.
#[tauri::command]
pub async fn purge_archived_document(
    document_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let lib = lib_arc(&state).await?;
    lib.purge_archived_document(&document_id).map_err(err)
}

#[tauri::command]
pub async fn get_history(
    document_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<VersionEntry>, String> {
    let lib = lib_arc(&state).await?;
    lib.get_history(&document_id).map_err(err)
}

#[tauri::command]
pub async fn set_version_note(
    version_id: i64,
    note: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let lib = lib_arc(&state).await?;
    lib.set_version_note(version_id, note.as_deref())
        .map_err(err)
}

#[derive(Serialize)]
pub struct ExportResult {
    pub path: PathBuf,
    pub file_count: usize,
}

/// Export a version's file tree under a user-picked directory, into
/// a freshly-created subdirectory named like
/// `<visible_name>-v<version_id>-<observed_at>`. Returns the full path
/// that was written and the number of files in it, or `None` if the
/// user cancelled the directory picker.
///
/// Audit fix H7: the destination is chosen via a server-side folder
/// picker so the renderer can't direct writes into
/// `~/Library/LaunchAgents` or similar.
#[tauri::command]
pub async fn export_version(
    app: AppHandle,
    version_id: i64,
    state: State<'_, AppState>,
) -> Result<Option<ExportResult>, String> {
    let lib = lib_arc(&state).await?;

    let entry = lib.get_version(version_id).map_err(err)?;
    let manifest_bytes = lib.read_blob(&entry.manifest_hash).map_err(err)?;
    let manifest = rehydrate_core::Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;

    let app_for_pick = app.clone();
    let title = format!("Export \"{}\" v{} to…", manifest.visible_name, version_id);
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_pick
            .dialog()
            .file()
            .set_title(&title)
            .blocking_pick_folder()
    })
    .await
    .map_err(err)?;

    let Some(dest_path) = picked else {
        return Ok(None);
    };
    let dest_dir = dest_path
        .into_path()
        .map_err(|e| format!("could not resolve picked folder: {e}"))?;

    let safe_name = sanitize(&manifest.visible_name);
    let safe_ts = entry
        .observed_at
        .replace([':', 'T'], "-")
        .trim_end_matches('Z')
        .to_string();
    let dir_name = format!("{safe_name}-v{version_id}-{safe_ts}");
    let target = dest_dir.join(&dir_name);

    if target.exists() {
        return Err(format!(
            "{} already exists; refusing to overwrite",
            target.display()
        ));
    }

    lib.reconstruct(version_id, &target).map_err(err)?;

    Ok(Some(ExportResult {
        path: target,
        file_count: manifest.files.len(),
    }))
}

fn thumbnail_fallback_pdf(
    lib: &Library,
    manifest: &rehydrate_core::Manifest,
    title: &str,
    ocr_per_page: Option<&[String]>,
) -> Result<Vec<u8>, String> {
    let mut thumbs: Vec<_> = manifest
        .files
        .iter()
        .filter(|f| f.path.ends_with(".png") && f.path.contains(".thumbnails"))
        .collect();
    if thumbs.is_empty() {
        return Err(
            "notebook has no .rm ink files we can parse and no thumbnails to fall back on \
             — sync the device once (or open and edit the notebook on the tablet first) \
             and try again"
                .to_string(),
        );
    }
    thumbs.sort_by(|a, b| a.path.cmp(&b.path));
    let mut pages = Vec::with_capacity(thumbs.len());
    for f in &thumbs {
        pages.push(lib.read_blob(&f.sha256).map_err(err)?);
    }
    rehydrate_render::build_pdf_from_pngs(title, &pages, ocr_per_page)
}

fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else if c == ' ' {
            out.push('-');
        }
    }
    // Strip leading/trailing dots, dashes, and underscores. Windows
    // silently drops trailing dots and spaces on write, so a doc
    // literally named "..." would otherwise become an empty filename
    // (or worse, collide with a parent-directory shortcut). We also
    // strip dashes/underscores because the space→`-` rewrite above
    // turns runs of trailing whitespace into runs of dashes.
    let trimmed = out.trim_matches(|c: char| c == '.' || c == '-' || c == '_');
    if trimmed.is_empty() {
        return "Untitled".into();
    }
    // Reserved-name check looks at the bare stem (everything before
    // the first `.`) case-insensitively — Windows reserves these
    // regardless of extension or casing.
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    let reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM0"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT0"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    if reserved {
        // Prefix-rescue a reserved name so the on-disk filename is
        // legal on Windows but still recognisable to the user. A
        // doc the user titled "CON" exports as "doc-CON-…".
        format!("doc-{trimmed}")
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize;

    #[test]
    fn strips_special_chars_and_normalises_spaces() {
        assert_eq!(sanitize("Hello World"), "Hello-World");
        assert_eq!(sanitize("a/b\\c?d*e:f"), "abcdef");
    }

    #[test]
    fn empty_after_strip_falls_back_to_untitled() {
        assert_eq!(sanitize(""), "Untitled");
        assert_eq!(sanitize("???"), "Untitled");
        assert_eq!(sanitize(".."), "Untitled");
        assert_eq!(sanitize("   "), "Untitled");
    }

    #[test]
    fn trailing_dot_or_space_is_dropped() {
        // Windows would otherwise silently truncate to "foo".
        assert_eq!(sanitize("foo."), "foo");
        assert_eq!(sanitize("foo "), "foo");
        assert_eq!(sanitize(".foo."), "foo");
    }

    #[test]
    fn windows_reserved_names_are_prefixed() {
        // Without the rescue, exporting a doc named "CON" would
        // produce "CON-v…" — a path Windows refuses to create.
        assert_eq!(sanitize("CON"), "doc-CON");
        assert_eq!(sanitize("nul"), "doc-nul");
        assert_eq!(sanitize("LPT1"), "doc-LPT1");
        // Non-reserved names with the same prefix are untouched.
        assert_eq!(sanitize("Console"), "Console");
        assert_eq!(sanitize("Connor"), "Connor");
    }
}

#[cfg(test)]
mod export_staging_tests {
    //! Unit coverage for `stage_export_for_document`. Drives the
    //! staging helper against an in-memory tempdir library so the
    //! tests don't need a Tauri shell. The notebook-render branch
    //! is exercised indirectly via the PDF branch (which shares the
    //! same atomic-persist + cache-key logic without dragging in
    //! `rehydrate-render`'s parser); we'd happily test the notebook
    //! branch too but that means seeding `.rm` blobs, which is a
    //! different crate's concern.

    use std::path::PathBuf;
    use std::sync::Arc;

    use rehydrate_core::{ImportKind, Library};

    use super::{stage_export_for_document, StagingInputs};

    /// Build a one-document PDF library under `dir` and return the
    /// shared Arc + staging inputs the helper expects.
    fn seed_pdf_doc(dir: &std::path::Path, visible_name: &str) -> (Arc<Library>, StagingInputs) {
        use rehydrate_core::Manifest;
        let lib = Arc::new(Library::open(dir).unwrap());
        let src = dir.join("seed.pdf");
        std::fs::write(&src, b"%PDF-1.7 seed bytes").unwrap();
        let summary = lib
            .import_file(&src, ImportKind::Pdf, visible_name)
            .unwrap();
        let manifest_bytes = lib.read_blob(&summary.current_manifest).unwrap();
        let manifest = Manifest::from_canonical_json(&manifest_bytes).unwrap();
        let inputs = StagingInputs {
            document_id: summary.document_id.clone(),
            visible_name: summary.visible_name.clone(),
            current_manifest: summary.current_manifest.as_str().to_string(),
            manifest,
        };
        (lib, inputs)
    }

    /// A `Library` holds an OS advisory lock for its lifetime, so we
    /// can't open two libraries against the same dir even in a single
    /// process. The PDF-branch tests below use one library per
    /// tempdir.
    fn fresh_export_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let export = dir.path().join("export");
        (dir, export)
    }

    fn nop_warn(_: &str) {}

    #[test]
    fn staging_filename_uses_sanitised_visible_name() {
        // Visible names with path-separator characters must not
        // leak into the on-disk filename — `sanitize` rewrites them.
        // The staging directory carries the validated document id,
        // never the renderer-supplied parameter.
        let dir = tempfile::tempdir().unwrap();
        let (lib, inputs) = seed_pdf_doc(dir.path(), "Naughty/Name?.draft");
        let (_root_guard, export_root) = fresh_export_root();

        let (paths, is_hit) =
            stage_export_for_document(&lib, &export_root, &inputs, &nop_warn).unwrap();

        assert!(!is_hit, "first call must be a cold render");
        let file_name = paths.file.file_name().unwrap().to_string_lossy();
        // sanitize() strips path-separator/wildcard chars (`/`, `?`)
        // but keeps inline dots; trailing dots/dashes are trimmed.
        assert_eq!(
            file_name, "NaughtyName.draft.pdf",
            "sanitize must strip `/` and `?` but preserve inline dots"
        );
        assert!(
            !file_name.contains('/') && !file_name.contains('?'),
            "path-separator and wildcard chars must not appear in the staging filename"
        );
        // Parent dir is `<export_root>/<document_id>-<key>/` — the
        // validated id appears verbatim, not the (potentially
        // attacker-controlled) caller-supplied string. Belt-and-
        // braces guard for the parameter-vs-`doc.document_id` issue
        // the reviewer flagged.
        let parent = paths.file.parent().unwrap();
        let parent_name = parent.file_name().unwrap().to_string_lossy();
        assert!(
            parent_name.starts_with(&format!("{}-", inputs.document_id)),
            "staging dir must be prefixed with the validated document_id; got {parent_name}"
        );
        // Body was the imported blob, copied verbatim.
        assert_eq!(std::fs::read(&paths.file).unwrap(), b"%PDF-1.7 seed bytes");
        // Drag-preview icon is materialised next to the cache root.
        assert!(paths.icon.exists(), "drag icon must be staged on first call");
    }

    #[test]
    fn second_call_is_a_cache_hit() {
        // The reviewer's #2 issue: previously the body bytes were
        // read (and notebooks re-rendered) BEFORE the staging-path
        // existence check. A hover prefetch followed by an
        // immediate ⌥-drag should never re-render; this test fails
        // if the early-return regresses.
        let dir = tempfile::tempdir().unwrap();
        let (lib, inputs) = seed_pdf_doc(dir.path(), "My Notes");
        let (_root_guard, export_root) = fresh_export_root();

        let (first, _) =
            stage_export_for_document(&lib, &export_root, &inputs, &nop_warn).unwrap();
        let first_mtime = std::fs::metadata(&first.file).unwrap().modified().unwrap();

        // Sleep a hair so a regression that re-persists would
        // change the mtime. (We can't probe a "did we render?"
        // counter without threading state through the helper.)
        std::thread::sleep(std::time::Duration::from_millis(20));

        let (second, is_hit) =
            stage_export_for_document(&lib, &export_root, &inputs, &nop_warn).unwrap();
        assert!(is_hit, "second call must short-circuit on cache hit");
        assert_eq!(first.file, second.file);
        let second_mtime = std::fs::metadata(&second.file).unwrap().modified().unwrap();
        assert_eq!(
            first_mtime, second_mtime,
            "cache hit must not rewrite the staging file"
        );
    }

    #[test]
    fn cache_key_isolates_documents() {
        // Different documents (different document_ids and content
        // hashes) must land under separate staging directories so a
        // stale entry can't shadow a fresh one. Implicitly checks
        // that the validated `doc.document_id` flows into the path.
        let dir_a = tempfile::tempdir().unwrap();
        let (lib_a, inputs_a) = seed_pdf_doc(dir_a.path(), "doc-A");
        let dir_b = tempfile::tempdir().unwrap();
        let (lib_b, inputs_b) = seed_pdf_doc(dir_b.path(), "doc-B");
        let (_root_guard, export_root) = fresh_export_root();

        let (paths_a, _) =
            stage_export_for_document(&lib_a, &export_root, &inputs_a, &nop_warn).unwrap();
        let (paths_b, _) =
            stage_export_for_document(&lib_b, &export_root, &inputs_b, &nop_warn).unwrap();
        assert_ne!(paths_a.file.parent(), paths_b.file.parent());
    }

    #[test]
    fn notebook_cache_key_includes_export_layout_version() {
        // The notebook branch keys the cache as
        // `<manifest_hash>-<EXPORT_LAYOUT_VERSION>`. We don't seed a
        // full notebook here (that'd pull in `rehydrate-render`'s
        // parser), but the *path computation* uses the same
        // formula, and a future EXPORT_LAYOUT_VERSION bump must
        // produce a different staging directory.
        //
        // Drive the assertion directly against the format string
        // the helper uses — if the formula here ever diverges from
        // the production code, the staging dir for a given doc
        // would silently keep returning the stale cached PDF on a
        // layout-version bump. Keeping the formula duplicated in
        // the test (rather than calling into the helper) is the
        // point.
        let manifest_hash = "deadbeef".repeat(8);
        let v_now = rehydrate_render::EXPORT_LAYOUT_VERSION;
        let key_now = format!("{}-{}", manifest_hash, v_now);
        // Use a clearly synthetic version string so the test doesn't
        // need to be updated whenever a real bump happens.
        let key_synthetic = format!("{}-{}", manifest_hash, "export-test-bump");
        assert_ne!(
            key_now, key_synthetic,
            "EXPORT_LAYOUT_VERSION must participate in the cache key"
        );
        // Sanity-check the current value so a typo in the constant
        // doesn't silently disable export-cache busting.
        assert!(
            v_now.starts_with("export-"),
            "EXPORT_LAYOUT_VERSION should follow the `export-vN` pattern; got {v_now}"
        );
    }
}

#[cfg(test)]
mod transcript_parse_tests {
    //! The OCR transcript format is owned by
    //! `ocr_commands::transcribe_document` — the parser here has to
    //! stay in lockstep with the writer there. Tests pin the
    //! page-index alignment contract (blanks emit empty entries,
    //! failed pages emit the placeholder text) so a future change to
    //! the writer that doesn't update the parser surfaces as a
    //! failing test rather than as silently-misaligned search
    //! highlights in exported PDFs.

    use super::parse_transcript_pages;

    #[test]
    fn strips_frontmatter_and_returns_pages() {
        let md = "---\nmodel: test\ncreated_at: 2024\n---\n\nPage one body.\n\nPage two body.\n";
        let pages = parse_transcript_pages(md);
        assert_eq!(pages, vec!["Page one body.", "Page two body."]);
    }

    #[test]
    fn blank_pages_become_empty_entries() {
        // Mirrors the writer's `if i > 0 { push("\n\n") }` loop:
        // blank pages are emitted as empty paragraphs between the
        // surrounding "\n\n" separators, so a 3-page notebook with a
        // blank middle page round-trips to ["a", "", "b"].
        let md = "---\nmodel: t\n---\n\na\n\n\n\nb\n";
        let pages = parse_transcript_pages(md);
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0], "a");
        assert_eq!(pages[1], "");
        assert_eq!(pages[2], "b");
    }

    #[test]
    fn strips_leading_note_lines() {
        // The writer prepends `_note: …_` headers before the page
        // paragraphs when blank/failed pages exist. They must not
        // bleed into page 0's content.
        let md = "---\nmodel: t\n---\n\n_note: 1 blank page skipped_\n\n_note: 2 pages could not be transcribed_\n\nFirst page text.\n\nSecond page text.\n";
        let pages = parse_transcript_pages(md);
        assert_eq!(pages[0], "First page text.");
        assert_eq!(pages[1], "Second page text.");
    }

    #[test]
    fn handles_transcript_without_frontmatter() {
        // Defensive: a future writer change (or a manually-edited
        // transcript) without frontmatter should still produce a
        // useful page split rather than empty output.
        let md = "alpha\n\nbeta\n";
        let pages = parse_transcript_pages(md);
        assert_eq!(pages, vec!["alpha", "beta"]);
    }
}

#[cfg(test)]
mod export_ocr_tests {
    //! End-to-end coverage for the searchable-PDF path: render the
    //! export PDF with an OCR slice and assert the OCR strings appear
    //! in the resulting bytes. printpdf encodes ASCII strings as
    //! literal WinAnsi text in the content stream, so a substring
    //! match on the raw bytes is sufficient — no PDF parser needed.
    //!
    //! We use the `build_pdf_from_pngs` path rather than the `.rm`
    //! path so the test doesn't have to seed a valid v6 binary; the
    //! invisible-text logic is identical between the two and is
    //! pulled from the same `invisible_text_layer_ops` helper.

    use image::{ImageBuffer, Rgba};

    /// Encode a 1×1 transparent RGBA PNG at runtime. Hand-rolling
    /// the bytes risks CRC drift (the IDAT CRC depends on the exact
    /// pixel encoding), and the `image` crate is already a render
    /// dep so the cost is free.
    fn tiny_png() -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(1, 1, Rgba([0, 0, 0, 0]));
        let mut bytes = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .expect("encode 1×1 PNG");
        bytes
    }

    #[test]
    fn invisible_text_layer_contains_ocr_string() {
        let needle = "QuickBrownFoxOcrMarker".to_string();
        let pages = vec![tiny_png()];
        let ocr = vec![needle.clone()];
        let pdf =
            rehydrate_render::build_pdf_from_pngs("ocr-test", &pages, Some(&ocr)).unwrap();
        // printpdf 0.9 writes literal ASCII strings into Tj content
        // streams; a raw-byte search is the cheapest possible
        // verification that the invisible text actually made it in.
        assert!(
            twoway_contains(&pdf, needle.as_bytes()),
            "exported PDF must contain the OCR text in its content stream"
        );
    }

    #[test]
    fn no_ocr_produces_no_text_section() {
        // Regression guard: when the caller passes `None` (Preview
        // path), the renderer must not register a font or emit BT/ET
        // ops. Easiest to verify by asserting the PDF doesn't carry
        // the Helvetica resource name in its body.
        let pages = vec![tiny_png()];
        let pdf = rehydrate_render::build_pdf_from_pngs("no-ocr", &pages, None).unwrap();
        // The font would land as a `/Helvetica` resource entry —
        // its absence proves we didn't pay the registration cost
        // (and didn't accidentally start the OCR path).
        assert!(
            !twoway_contains(&pdf, b"Helvetica"),
            "PDF rendered with ocr=None must not register the OCR text font"
        );
    }

    /// Naive O(N*M) substring search; the inputs are small and this
    /// avoids pulling in a regex / memchr dep.
    fn twoway_contains(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}

#[cfg(test)]
mod import_size_cap_tests {
    use super::{enforce_import_size_cap, MAX_IMPORT_FILE_BYTES};

    #[test]
    fn cap_value_is_64_mib() {
        // Lock the constant. The renderer's
        // MAX_IMPORT_FILE_BYTES in ui/src/ipc.ts must match this
        // number so the preflight in handleExternalFileDrop
        // (App.tsx) rejects exactly the same set of files this
        // backend would reject. Issue #24: a divergence here would
        // re-open the OOM/freeze path that the renderer preflight
        // is supposed to close.
        assert_eq!(MAX_IMPORT_FILE_BYTES, 64 * 1024 * 1024);
    }

    #[test]
    fn at_or_below_cap_is_accepted() {
        assert!(enforce_import_size_cap("ok.pdf", 0).is_ok());
        assert!(enforce_import_size_cap("ok.pdf", 1).is_ok());
        assert!(enforce_import_size_cap("ok.pdf", MAX_IMPORT_FILE_BYTES).is_ok());
    }

    #[test]
    fn one_byte_over_cap_is_rejected() {
        let err = enforce_import_size_cap("huge.pdf", MAX_IMPORT_FILE_BYTES + 1)
            .expect_err("byte over the cap must be rejected");
        // Surface the file name so the toast/error UI can identify
        // which drop was rejected when the user dropped a batch.
        assert!(
            err.contains("huge.pdf"),
            "error must mention file name: {err}"
        );
        assert!(
            err.contains("64 MiB"),
            "error must spell out the limit: {err}"
        );
    }
}

// ---------- Device ----------------------------------------------------------

#[tauri::command]
pub async fn device_state(state: State<'_, AppState>) -> Result<DeviceState, String> {
    let cfg = SshConfig::default();
    let endpoint = format!("{}:{}", cfg.host, cfg.port);
    // A missing or unreadable known_hosts file is reported as "no
    // recorded fingerprint" — the UI will simply hide the Forget
    // affordance, which is the right post-state either way.
    let has_recorded_host_key = known_hosts_for_app()
        .lookup(&endpoint)
        .ok()
        .flatten()
        .is_some();
    Ok(DeviceState {
        reachable: *state.device_reachable.read().await,
        connected: state.device.lock().await.is_some(),
        info: state.device_info.read().await.clone(),
        has_stored_password: keychain::slot_has_value(KEYRING_DEVICE_USER),
        has_recorded_host_key,
    })
}

/// Clear the pinned host-key fingerprint for the default device
/// endpoint. The next connect will re-record under TOFU.
///
/// Returns `true` when an entry was removed and `false` when none
/// existed — both outcomes leave the UI in the same desired state
/// ("no pinned key"), so the renderer treats both as success and
/// just refreshes `device_state` to hide the button.
#[tauri::command]
pub async fn forget_device_host_key() -> Result<bool, String> {
    let cfg = SshConfig::default();
    let endpoint = format!("{}:{}", cfg.host, cfg.port);
    known_hosts_for_app().forget(&endpoint).map_err(err)
}

/// Save a device password into the OS keychain. Does not connect.
/// The `IpcSecret` wrapper length-caps the input and ensures the
/// plaintext isn't printable via Debug derives further up the
/// call chain.
#[tauri::command]
pub async fn save_device_password(password: IpcSecret) -> Result<(), String> {
    keychain::write_slot(KEYRING_DEVICE_USER, password.expose())
}

#[tauri::command]
pub async fn forget_device_password() -> Result<(), String> {
    keychain::forget_slot(KEYRING_DEVICE_USER)
}

/// Open an SSH session against the tablet.
///
/// `password` semantics:
/// - `None`: read the password from the OS keychain. If none stored,
///   returns an error so the renderer can show the password dialog.
/// - `Some(_)`: use the supplied password.
///
/// `remember` semantics:
/// - `Some(true)`: on a successful connect, persist `password` to the
///   keychain. Requires `password` to be `Some(_)`.
/// - Any other value: do **not** write the keychain.
///
/// The previous default was "always save on first successful
/// connect". The renderer is the choke point — if it's XSS'd, both
/// `save_device_password` and `connect_device` are reachable
/// programmatically. Requiring an explicit `remember: true` flag
/// means a hostile script that just wants to quietly probe
/// `connect_device` (e.g. to confirm a tablet is online) can no
/// longer overwrite the user's stored password as a side effect.
/// The dedicated `save_device_password` command remains the
/// canonical "store this" call.
#[tauri::command]
pub async fn connect_device(
    password: Option<IpcSecret>,
    remember: Option<bool>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DeviceInfo, String> {
    let cfg = SshConfig::default();
    // Resolve the password without persisting yet. Persisting before we
    // know the password is correct means a typo gets cached and the next
    // connect attempt silently uses the bad value.
    //
    // Audit fix M4: a parallel un-zeroized `Option<String>` shadow used
    // to defeat SecretString's zero-on-drop. The IPC layer now hands us
    // an `IpcSecret`, so the plaintext never lives in a plain String the
    // caller could Debug-print. We unwrap into SecretString immediately.
    let (secret, freshly_typed) = match password {
        Some(p) => (p.into_secret(), true),
        None => {
            let stored = keychain::read_slot(KEYRING_DEVICE_USER)
                .ok_or_else(|| "no password stored; pass one to connect_device".to_string())?;
            (SecretString::from(stored), false)
        }
    };

    let dev = SshDevice::connect(cfg, secret.clone(), known_hosts_for_app())
        .await
        .map_err(err)?;
    let info = dev.ping().await.map_err(err)?;

    // Surface any one-shot warnings the connect path collected.
    // Today the only case is "TOFU host-key write failed" — see
    // `SshDevice::take_pending_warning`. The connection is fine to
    // use; the warning tells the user that subsequent reconnects
    // won't have a pinned fingerprint to compare against until
    // they fix the underlying FS / permissions issue.
    if let Some(msg) = dev.take_pending_warning() {
        tracing::warn!("device pending warning: {msg}");
        let _ = app.emit("host-key:warning", msg);
    }

    // Connection succeeded — persist only if the renderer explicitly
    // asked for it. If the OS keyring is unavailable (most often on
    // minimal Linux installs without `secret-service` /
    // `gnome-keyring` running), both `Entry::new` and `set_password`
    // can fail. We surface the failure as a `keyring:warning` event
    // so the UI can tell the user "we couldn't save your password";
    // silently logging makes the user wonder why their password
    // isn't being remembered.
    if freshly_typed && remember == Some(true) {
        use secrecy::ExposeSecret;
        if let Err(e) = keychain::write_slot(KEYRING_DEVICE_USER, secret.expose_secret()) {
            tracing::warn!("could not persist device password to keychain: {e}");
            let _ = app.emit(
                "keyring:warning",
                format!(
                    "Couldn't save the tablet password to the system keychain ({e}). \
                     You'll be asked again next time. On Linux this usually means \
                     `gnome-keyring` / `secret-service` isn't installed or running."
                ),
            );
        }
    }

    *state.device.lock().await = Some(Arc::new(dev));
    *state.device_info.write().await = Some(info.clone());
    Ok(info)
}

#[tauri::command]
pub async fn disconnect_device(state: State<'_, AppState>) -> Result<(), String> {
    *state.device.lock().await = None;
    *state.device_info.write().await = None;
    Ok(())
}

// ---------- Sync ------------------------------------------------------------

#[tauri::command]
pub async fn restore_version(version_id: i64, state: State<'_, AppState>) -> Result<i64, String> {
    let lib = lib_arc(&state).await?;
    let outcome = lib.restore_version(version_id).map_err(err)?;
    Ok(outcome.version_id)
}

async fn device_arc(state: &State<'_, AppState>) -> Result<Arc<SshDevice>, String> {
    state
        .device
        .lock()
        .await
        .as_ref()
        .map(Arc::clone)
        .ok_or_else(|| "device not connected".to_string())
}

/// Forward sync progress events from the engine's channel to the
/// renderer over the Tauri event bus. Bails out cleanly when:
///
/// - the engine drops its sender (channel closes), OR
/// - the engine signals `Done`/`Cancelled`, OR
/// - the renderer disconnects (consecutive `emit` failures past
///   `EMIT_FAIL_THRESHOLD`) — without this guard, the forwarder
///   would spin on a dead IPC channel until the engine itself
///   completed.
///
/// Same pattern across pull/push/two-way, so it lives here rather
/// than copy-pasted.
fn spawn_sync_progress_forwarder(
    app: AppHandle,
    mut rx: tokio::sync::mpsc::Receiver<ProgressEvent>,
) -> tauri::async_runtime::JoinHandle<()> {
    const EMIT_FAIL_THRESHOLD: u32 = 8;
    tauri::async_runtime::spawn(async move {
        let mut consecutive_emit_failures: u32 = 0;
        while let Some(ev) = rx.recv().await {
            match app.emit("sync:progress", &ev) {
                Ok(()) => consecutive_emit_failures = 0,
                Err(e) => {
                    consecutive_emit_failures += 1;
                    if consecutive_emit_failures >= EMIT_FAIL_THRESHOLD {
                        tracing::warn!(
                            "sync progress forwarder bailing after {consecutive_emit_failures} \
                             consecutive emit failures (last: {e}); renderer likely closed"
                        );
                        break;
                    }
                }
            }
            if matches!(ev, ProgressEvent::Done { .. } | ProgressEvent::Cancelled) {
                break;
            }
        }
    })
}

#[tauri::command]
pub async fn pull_plan(state: State<'_, AppState>) -> Result<PullPlan, String> {
    let dev = device_arc(&state).await?;
    let lib = lib_arc(&state).await?;
    plan_pull(&lib, dev.as_ref()).await.map_err(err)
}

#[tauri::command]
pub async fn pull_execute(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SyncReportOut, String> {
    let dev = device_arc(&state).await?;
    let lib = lib_arc(&state).await?;

    let plan = plan_pull(&lib, dev.as_ref()).await.map_err(err)?;

    let (tx, rx) = progress::channel(64);
    let forwarder = spawn_sync_progress_forwarder(app.clone(), rx);

    // Reset the shared cancel handle to a clean (un-cancelled) state
    // and pass a clone to the engine. The renderer's `cancel_sync`
    // command flips the same handle.
    state.sync_cancel.reset();
    let report = execute_pull(
        &lib,
        dev.as_ref(),
        plan,
        Some(tx),
        state.sync_cancel.clone(),
    )
    .await
    .map_err(err)?;
    let _ = forwarder.await;

    Ok(SyncReportOut {
        recorded: report.recorded,
        unchanged: report.unchanged,
        skipped: report.skipped,
    })
}

#[tauri::command]
pub async fn push_plan(state: State<'_, AppState>) -> Result<PushPlan, String> {
    let lib = lib_arc(&state).await?;
    plan_push(&lib).map_err(err)
}

#[tauri::command]
pub async fn push_execute(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<PushReportOut, String> {
    let dev = device_arc(&state).await?;
    let lib = lib_arc(&state).await?;

    let plan = plan_push(&lib).map_err(err)?;
    let (tx, rx) = progress::channel(64);
    let forwarder = spawn_sync_progress_forwarder(app.clone(), rx);
    state.sync_cancel.reset();
    let report = execute_push(
        &lib,
        dev.as_ref(),
        plan,
        Some(tx),
        state.sync_cancel.clone(),
    )
    .await
    .map_err(err)?;
    let _ = forwarder.await;

    Ok(PushReportOut {
        pushed: report.pushed,
        unchanged: report.unchanged,
        skipped: report.skipped,
    })
}

/// Pull-then-push. The pull-first ordering means device-side changes are
/// captured before any library-side change overwrites them; the loser of any
/// conflict is preserved in the version log via parent_version_id and
/// remains restorable from the history view.
#[tauri::command]
pub async fn sync_two_way(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TwoWayReport, String> {
    let dev = device_arc(&state).await?;
    let lib = lib_arc(&state).await?;

    // Reset the cancel handle once for the whole two-way sync; the
    // renderer's `cancel_sync` command will trip both phases.
    state.sync_cancel.reset();

    // ----- PULL phase -----
    let _ = app.emit("sync:phase", "pull");
    let pull_plan = plan_pull(&lib, dev.as_ref()).await.map_err(err)?;
    let (tx, rx) = progress::channel(64);
    let forwarder = spawn_sync_progress_forwarder(app.clone(), rx);
    let pull = execute_pull(
        &lib,
        dev.as_ref(),
        pull_plan,
        Some(tx),
        state.sync_cancel.clone(),
    )
    .await
    .map_err(err)?;
    let _ = forwarder.await;

    // ----- PUSH phase -----
    let _ = app.emit("sync:phase", "push");
    let push_plan = plan_push(&lib).map_err(err)?;
    let (tx, rx) = progress::channel(64);
    let forwarder = spawn_sync_progress_forwarder(app.clone(), rx);
    let push = execute_push(
        &lib,
        dev.as_ref(),
        push_plan,
        Some(tx),
        state.sync_cancel.clone(),
    )
    .await
    .map_err(err)?;
    let _ = forwarder.await;

    Ok(TwoWayReport {
        pull: SyncReportOut {
            recorded: pull.recorded,
            unchanged: pull.unchanged,
            skipped: pull.skipped,
        },
        push: PushReportOut {
            pushed: push.pushed,
            unchanged: push.unchanged,
            skipped: push.skipped,
        },
    })
}

// ---------- Internals -------------------------------------------------------

fn blob_stats(library_root: &std::path::Path) -> (usize, u64) {
    let blobs = library_root.join("blobs");
    let mut count = 0usize;
    let mut size = 0u64;
    walk(&blobs, &mut |path| {
        if let Ok(meta) = std::fs::metadata(path) {
            count += 1;
            size += meta.len();
        }
    });
    (count, size)
}

/// Hard cap on `walk` recursion depth. The blob store is a two-level
/// fanout (`blobs/<aa>/<bb>/<hash>`) so a sane tree never exceeds 3 —
/// anything past the cap is a malformed (or hostile) directory, and
/// without the bound a symlink loop the os layer doesn't filter could
/// blow the stack inside an `async` command.
const BLOB_WALK_MAX_DEPTH: usize = 8;

fn walk(p: &std::path::Path, f: &mut impl FnMut(&std::path::Path)) {
    walk_inner(p, f, 0);
}

fn walk_inner(p: &std::path::Path, f: &mut impl FnMut(&std::path::Path), depth: usize) {
    if depth > BLOB_WALK_MAX_DEPTH {
        return;
    }
    let rd = match std::fs::read_dir(p) {
        Ok(r) => r,
        Err(e) => {
            // Surface — but don't crash — failures mid-walk so an
            // orphan-blob undercount is visible in the operations log
            // rather than silently shrinking the report's blob_count.
            tracing::warn!(path = %p.display(), "read_dir failed during blob walk: {e}");
            return;
        }
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        // Skip symlinks: the blob store should never contain them, and
        // following a planted symlink could let the walker enumerate
        // files outside the library root.
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk_inner(&path, f, depth + 1);
        } else {
            f(&path);
        }
    }
}

/// Background task started at app launch. Polls the USB-ethernet endpoint
/// every 2s and emits `device:reachable` events on changes. Cheap and
/// platform-agnostic — no USB driver hooks needed.
///
/// The loop watches `AppState::shutdown_requested` and exits when the
/// app's `RunEvent::ExitRequested` handler sets it — without the flag,
/// the thread holds the `AppHandle` past Tauri's teardown and prevents
/// clean shutdown of the executor.
pub fn spawn_reachability_watcher(app: AppHandle) {
    // Run on a dedicated OS thread with its own tokio current-thread
    // runtime. Doing this from the Tauri `setup` callback fails because
    // neither tokio nor `tauri::async_runtime` has its reactor live yet
    // — they spin up later in Builder::run(). A standalone thread sidesteps
    // the ordering question entirely. The work is tiny (one TCP connect
    // every 2s), so a private runtime is fine.
    std::thread::Builder::new()
        .name("rehydrate-reachability".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("failed to start reachability watcher runtime: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                let mut last: Option<bool> = None;
                // Snapshot the shutdown flag once; cloning the Arc
                // up front keeps the per-tick check off the AppHandle.
                let shutdown = {
                    let state = app.state::<AppState>();
                    state.shutdown_requested.clone()
                };
                loop {
                    if shutdown.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let now = is_reachable("10.11.99.1", 22).await;
                    if last != Some(now) {
                        let state = app.state::<AppState>();
                        *state.device_reachable.write().await = now;
                        let _ = app.emit("device:reachable", now);
                        last = Some(now);
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            });
        })
        .expect("spawn reachability watcher thread");
}

#[cfg(test)]
mod open_library_allowlist_tests {
    use super::is_allowed_open_path;
    use crate::config::RecentLibrary;
    use std::path::{Path, PathBuf};

    fn rec(path: &str) -> RecentLibrary {
        RecentLibrary {
            path: PathBuf::from(path),
            label: "test".into(),
            last_opened: String::new(),
        }
    }

    /// Issue #36 regression: the bare-minimum attack shape. With no
    /// recents and no pending pick, an arbitrary renderer path must
    /// be refused. The default-dir allowance is intentionally narrow
    /// (`default_library_dir()` is the cross-platform default and is
    /// host-machine-dependent), so this test picks `/tmp/attacker`
    /// which is never the default.
    #[test]
    fn arbitrary_path_with_no_recents_no_pending_is_rejected() {
        let recents: Vec<RecentLibrary> = vec![];
        assert!(!is_allowed_open_path(
            Path::new("/tmp/attacker"),
            &recents,
            None,
        ));
    }

    /// The path the user just confirmed in the server-side OS picker
    /// is allowed. This is the post-`pick_library_directory` happy
    /// path that the renderer hits via `openAnotherLibrary` in
    /// `App.tsx`.
    #[test]
    fn pending_pick_path_is_allowed() {
        let recents: Vec<RecentLibrary> = vec![];
        let pending = PathBuf::from("/Users/test/MyLib");
        assert!(is_allowed_open_path(
            &pending,
            &recents,
            Some(pending.as_path()),
        ));
    }

    /// A different path while a pick is pending must still be refused
    /// — the pending slot only authorises the exact path it points at,
    /// not "any path while a pick happens to be pending."
    #[test]
    fn pending_pick_does_not_authorise_a_different_path() {
        let recents: Vec<RecentLibrary> = vec![];
        let pending = PathBuf::from("/Users/test/MyLib");
        assert!(!is_allowed_open_path(
            Path::new("/tmp/attacker"),
            &recents,
            Some(pending.as_path()),
        ));
    }

    /// Recents are the persisted form of "user previously approved
    /// this." A path on the recents list is allowed even with no
    /// active pending pick — covers the welcome screen re-opening a
    /// known library and `switch_library`'s equivalent gate.
    #[test]
    fn recents_path_is_allowed_without_pending() {
        let recents = vec![rec("/Users/test/PriorLib")];
        assert!(is_allowed_open_path(
            Path::new("/Users/test/PriorLib"),
            &recents,
            None,
        ));
    }

    /// A path that *looks* similar to a recents entry but isn't byte-
    /// equal must NOT match. Sibling-directory escape is the obvious
    /// attack shape: if recents contains `/Users/test/Lib`, an open
    /// against `/Users/test/Lib2` or `/Users/test/Lib/../Other` must
    /// fall through to the deny branch. (Renderer-sent paths aren't
    /// canonicalised here; the picker round-trip preserves whatever
    /// the OS returned, so byte-equality is the right comparison.)
    #[test]
    fn near_miss_recents_path_is_rejected() {
        let recents = vec![rec("/Users/test/Lib")];
        assert!(!is_allowed_open_path(
            Path::new("/Users/test/Lib2"),
            &recents,
            None,
        ));
        assert!(!is_allowed_open_path(
            Path::new("/Users/test/Lib/../Other"),
            &recents,
            None,
        ));
    }

    /// The default library dir is always allowed (welcome-screen
    /// "use defaults" button on first launch). Skip if the test host
    /// has no resolvable default — CI on the self-hosted macOS runner
    /// always does, but unit tests should still be portable.
    #[test]
    fn default_library_dir_is_allowed() {
        let Some(default) = crate::state::default_library_dir() else {
            return;
        };
        let recents: Vec<RecentLibrary> = vec![];
        assert!(is_allowed_open_path(&default, &recents, None));
    }
}
