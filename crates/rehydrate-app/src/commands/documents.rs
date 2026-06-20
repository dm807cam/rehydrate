use std::path::PathBuf;

use rehydrate_core::{
    ArchiveReason, ArchivedDocument, DocumentSummary, Manifest, VersionEntry,
};
use rehydrate_device::Device;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_opener::OpenerExt;

use super::util::{sanitize, thumbnail_fallback_pdf};
use crate::state::AppState;
use crate::util::{err, lib_arc};

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
///
/// If the device is connected, also hard-deletes the document from the
/// device file system. This prevents a locally-archived doc that was pushed
/// with `deleted: true` (and is therefore still sitting in the tablet's
/// Trash) from being re-downloaded on the next sync and reappearing in the
/// "Tablet Trash" view. The device call is best-effort: if the device is
/// not connected, or the file is already gone, the local purge still
/// succeeds and no error is surfaced.
#[tauri::command]
pub async fn purge_archived_document(
    document_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let lib = lib_arc(&state).await?;
    lib.purge_archived_document(&document_id).map_err(err)?;

    // Best-effort immediate device cleanup. If it succeeds, clear the
    // queue entry so the pull engine doesn't redundantly retry. If the
    // device is unreachable or the call fails, the queue entry remains
    // and execute_pull will delete the file on the next sync.
    let guard = state.device.lock().await;
    if let Some(dev) = guard.as_ref() {
        match dev.delete_document_tree(&document_id).await {
            Ok(()) => {
                tracing::info!(uuid = %document_id, "purge_archived_document: removed from device");
                let _ = lib.dequeue_device_deletion(&document_id);
            }
            Err(e) => {
                tracing::debug!(
                    uuid = %document_id,
                    error = %e,
                    "purge_archived_document: immediate device cleanup failed, queued for next sync"
                );
            }
        }
    }
    Ok(())
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
                rehydrate_render::build_pdf_from_rm_files(&doc.visible_name, &bufs).or_else(
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
                        thumbnail_fallback_pdf(&lib, &manifest, &doc.visible_name)
                    },
                )?
            } else {
                thumbnail_fallback_pdf(&lib, &manifest, &doc.visible_name)?
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
