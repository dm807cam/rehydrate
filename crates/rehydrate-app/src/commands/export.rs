use std::path::PathBuf;
use std::sync::Arc;

use rehydrate_core::{Library, Manifest};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;

use super::util::{sanitize, thumbnail_fallback_pdf};
use crate::state::AppState;
use crate::util::{err, lib_arc};

/// Path bundle returned by [`prepare_export_pdf`]. The `file` is the
/// staged document — the path the JS side hands to
/// `@crabnebula/tauri-plugin-drag`'s `startDrag`. The `icon` is the
/// drag preview image the plugin requires; we stage the app's 32×32
/// PNG once into the export cache root so the renderer can pass a
/// real filesystem path even though the icon lives inside the
/// macOS app bundle at build time.
#[derive(Serialize)]
pub struct ExportDragPaths {
    pub file: PathBuf,
    pub icon: PathBuf,
}

#[derive(Serialize)]
pub struct ExportResult {
    pub path: PathBuf,
    pub file_count: usize,
}

/// Bytes for the 32×32 app icon, copied into the export cache at
/// first use so the drag-source plugin has a stable filesystem path
/// for the drag preview. Bundled at build time so a corrupted /
/// missing bundle resource doesn't break the drag.
const EXPORT_DRAG_ICON_PNG: &[u8] = include_bytes!("../../icons/32x32.png");

/// Stage a document on disk under a clean, human-friendly filename
/// so the renderer can hand the path to `tauri-plugin-drag` and the
/// OS sees a real file dropping onto the Desktop (or Finder, Mail,
/// etc.). Mirrors the body-type branching in `open_document`:
///
/// * PDF/EPUB documents → the stored blob is copied verbatim (no
///   re-encoding).
/// * Notebooks → `rehydrate-render` renders the `.rm` strokes to a
///   multi-page PDF (same renderer the Preview cache uses).
///
/// The staging file lives inside a per-key directory so the filename
/// itself is the clean, dropped-as-is name — Finder will land
/// "My Notes.pdf" rather than the hash-suffixed cache name. The
/// directory is the unit of cache-busting:
/// `~/.cache/rehydrate/export/{document_id}-{key}/{visible_name}.{ext}`.
///
/// Idempotent and safe to call from a hover handler: a cache hit
/// returns the existing paths without re-rendering. The renderer
/// pre-warms on `onMouseEnter` so dragstart is usually instant.
#[tauri::command]
pub async fn prepare_export_pdf(
    document_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ExportDragPaths, String> {
    enum ExportSource {
        Body {
            sha256: rehydrate_core::Sha256Hex,
            /// Stroke the document's `.rm` annotation layers on top of the
            /// body before staging. Only PDFs (never EPUBs, which we can't
            /// composite onto) opt in.
            overlay_annotations: bool,
        },
        Notebook,
    }

    let lib = lib_arc(&state).await?;
    let docs = lib.list_documents().map_err(err)?;
    let doc = docs
        .iter()
        .find(|d| d.document_id == document_id)
        .ok_or_else(|| format!("document {document_id} not in library"))?;
    let manifest_bytes = lib.read_blob(&doc.current_manifest).map_err(err)?;
    let manifest = Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;

    let export_root = directories::ProjectDirs::from("app", "rehydrate", "reHydrate")
        .map(|d| d.cache_dir().to_path_buf())
        .ok_or_else(|| "no cache directory on this platform".to_string())?
        .join("export");

    let safe_name = sanitize(&doc.visible_name);

    // Resolve enough metadata to compute the staging path first. On a
    // hot cache hit this returns without re-reading a large PDF/EPUB
    // blob or re-rendering a notebook, which is what keeps the
    // hover-prefetch path useful for macOS' short drag gesture window.
    let (ext, key, source) = if let Some(body) = manifest
        .files
        .iter()
        .find(|f| f.path.ends_with(".pdf") || f.path.ends_with(".epub"))
    {
        // Same defensive extension allow-list as open_document — a
        // crafted manifest can't extend the dropped filename to e.g.
        // `.command`.
        if body.path.ends_with(".pdf") {
            // PDF: stroke any annotation layers on top before staging, so a
            // drag-out matches what the reMarkable app (and the menu
            // `export_as_pdfs` path) produce. Key by manifest hash +
            // layout version, not the body hash alone: the old body-hash
            // key couldn't see `.rm` ink changes, so an annotated PDF was
            // cached — and dragged out — as the bare source document.
            let key = format!(
                "{}-{}",
                doc.current_manifest.as_str(),
                rehydrate_render::EXPORT_LAYOUT_VERSION,
            );
            (
                "pdf",
                key,
                ExportSource::Body {
                    sha256: body.sha256.clone(),
                    overlay_annotations: true,
                },
            )
        } else {
            // EPUB is copied verbatim; its content hash is a sufficient key.
            (
                "epub",
                body.sha256.as_str().to_string(),
                ExportSource::Body {
                    sha256: body.sha256.clone(),
                    overlay_annotations: false,
                },
            )
        }
    } else {
        // Notebook. The cache key combines manifest hash (covers all
        // ink + typed-text + transcript state via Manifest's content
        // hash) with EXPORT_LAYOUT_VERSION (covers renderer
        // changes). Independent of the Preview cache key — a future
        // export-only change (e.g. an invisible OCR text layer)
        // won't invalidate Preview PDFs and vice-versa.
        let key = format!(
            "{}-{}",
            doc.current_manifest.as_str(),
            rehydrate_render::EXPORT_LAYOUT_VERSION,
        );
        ("pdf", key, ExportSource::Notebook)
    };

    // Per-document staging dir: the filename inside is the clean
    // human name, so OS drop targets see "My Notes.pdf" rather than
    // a hash-suffixed cache file. Directory name carries the
    // content key (so a stale entry can't shadow a refreshed one
    // from a different document or manifest).
    let staging_dir = export_root.join(format!("{document_id}-{key}"));
    let staging_path = staging_dir.join(format!("{safe_name}.{ext}"));

    if !staging_path.exists() {
        let source_bytes = match source {
            ExportSource::Body {
                sha256,
                overlay_annotations,
            } => {
                let bytes = lib.read_blob(&sha256).map_err(err)?;
                if overlay_annotations {
                    // Compositing parses every annotation `.rm` and rewrites
                    // the PDF via lopdf — do it off the async runtime so the
                    // hover-prefetch doesn't stall the UI thread. On any
                    // failure we fall back to the bare body rather than
                    // breaking the drag.
                    let manifest_for = manifest.clone();
                    let doc_id_for = document_id.clone();
                    let lib_for = Arc::clone(&lib);
                    tauri::async_runtime::spawn_blocking(move || -> Vec<u8> {
                        match crate::commands::pdf_annotation_plan(
                            &manifest_for,
                            &doc_id_for,
                            &lib_for,
                        ) {
                            Ok(plan) if !plan.is_empty() => {
                                match rehydrate_render::assemble_annotated_pdf(&bytes, &plan) {
                                    Ok(assembled) => assembled,
                                    Err(e) => {
                                        tracing::warn!(
                                            "export overlay failed for {doc_id_for}: {e}"
                                        );
                                        bytes
                                    }
                                }
                            }
                            Ok(_) => bytes,
                            Err(e) => {
                                tracing::warn!(
                                    "export annotation plan failed for {doc_id_for}: {e}"
                                );
                                bytes
                            }
                        }
                    })
                    .await
                    .map_err(err)?
                } else {
                    bytes
                }
            }
            ExportSource::Notebook => {
                // Build the PDF on a worker thread — parsing 100s of pages
                // of `.rm` and stroking each one would otherwise block the
                // tokio runtime for several seconds on a cold render. The
                // hover prefetch is what keeps the actual dragstart cheap;
                // this branch only does real work the first time per
                // (document, manifest, layout-version) tuple.
                let render_doc_id = doc.document_id.clone();
                let render_visible_name = doc.visible_name.clone();
                let render_manifest = manifest.clone();
                let render_lib = Arc::clone(&lib);
                let app_for_warn = app.clone();
                tauri::async_runtime::spawn_blocking(move || -> Result<Vec<u8>, String> {
                    let mut rm_pages: Vec<_> = render_manifest
                        .files
                        .iter()
                        .filter(|f| {
                            f.path.ends_with(".rm")
                                && !f.path.contains(".thumbnails")
                                && !f.path.ends_with(".local")
                        })
                        .collect();
                    rm_pages.sort_by(|a, b| a.path.cmp(&b.path));
                    if !rm_pages.is_empty() {
                        let mut bufs = Vec::with_capacity(rm_pages.len());
                        for f in &rm_pages {
                            bufs.push(render_lib.read_blob(&f.sha256).map_err(err)?);
                        }
                        match rehydrate_render::build_pdf_from_rm_files(&render_visible_name, &bufs)
                        {
                            Ok(b) => Ok(b),
                            Err(e) => {
                                // Same legacy-format warning open_document
                                // emits — keep the user-visible signal
                                // consistent across the two entry points.
                                tracing::warn!(
                                    "export rendering failed for {render_doc_id}: {e}; falling back to thumbnails"
                                );
                                let _ = app_for_warn.emit(
                                    "document:legacy-format-warning",
                                    format!(
                                        "\"{render_visible_name}\" uses an older notebook format. The export falls \
                                         back to lower-resolution thumbnails. Sync the tablet to upgrade the \
                                         notebook to the current format.",
                                    ),
                                );
                                thumbnail_fallback_pdf(
                                    &render_lib,
                                    &render_manifest,
                                    &render_visible_name,
                                )
                            }
                        }
                    } else {
                        thumbnail_fallback_pdf(&render_lib, &render_manifest, &render_visible_name)
                    }
                })
                .await
                .map_err(err)??
            }
        };

        std::fs::create_dir_all(&staging_dir).map_err(err)?;
        // Write atomically: a same-dir tempfile + rename means the
        // OS never sees a partially-written staging file even if the
        // renderer crashes mid-write. Critical because the drag
        // gesture can fire immediately after this command resolves;
        // a half-written PDF would surface as a corrupt drop.
        let mut tmp = tempfile::NamedTempFile::new_in(&staging_dir).map_err(err)?;
        use std::io::Write;
        tmp.write_all(&source_bytes).map_err(err)?;
        tmp.persist(&staging_path)
            .map_err(|e| format!("persist staging file: {e}"))?;
    }

    // Stage the drag-preview icon once per export-cache root. The
    // plugin requires a filesystem path for the icon (its NSImage
    // initializer reads from disk on macOS); the app's PNG icon
    // ships compiled into the binary, so we materialise it next to
    // the staged document on demand. Cheap (1 KB write) and only
    // happens on first export per cache root.
    let icon_path = export_root.join(".drag-icon.png");
    if !icon_path.exists() {
        std::fs::create_dir_all(&export_root).map_err(err)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&export_root).map_err(err)?;
        use std::io::Write;
        tmp.write_all(EXPORT_DRAG_ICON_PNG).map_err(err)?;
        // Race tolerance: two parallel `prepare_export_pdf` calls
        // could both reach this branch and the second persist would
        // overwrite the first; that's fine because the icon bytes
        // are identical. `persist` clobbers on Unix anyway.
        tmp.persist(&icon_path)
            .map_err(|e| format!("persist drag icon: {e}"))?;
    }

    Ok(ExportDragPaths {
        file: staging_path,
        icon: icon_path,
    })
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
    let manifest = Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;

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

/// Build the page assembly plan for `rehydrate_render::assemble_annotated_pdf`.
///
/// Reads the document's `.content` blob to get the page ordering — including
/// notebook pages the user inserted *between* PDF pages — and pairs each with
/// its `.rm` ink layer (if any). Returns an empty vec when the document has no
/// `.content` page list (the caller then leaves the PDF untouched).
pub(crate) fn pdf_annotation_plan(
    manifest: &Manifest,
    doc_id: &str,
    lib: &Library,
) -> Result<Vec<rehydrate_render::PageSpec>, String> {
    let content_path = format!("{doc_id}.content");
    let Some(cf) = manifest.files.iter().find(|f| f.path == content_path) else {
        return Ok(vec![]);
    };

    let content_bytes = lib.read_blob(&cf.sha256).map_err(|e| e.to_string())?;
    let content_json: serde_json::Value = match serde_json::from_slice(&content_bytes) {
        Ok(v) => v,
        Err(_) => return Ok(vec![]),
    };

    // Map page UUID -> sha256 for all .rm files belonging to this document.
    let rm_prefix = format!("{doc_id}/");
    let rm_map: std::collections::HashMap<&str, _> = manifest
        .files
        .iter()
        .filter(|f| f.path.starts_with(&rm_prefix) && f.path.ends_with(".rm"))
        .filter_map(|f| {
            let rest = f.path.strip_prefix(&rm_prefix)?;
            let uuid = rest.strip_suffix(".rm")?;
            Some((uuid, &f.sha256))
        })
        .collect();

    let mut plan = Vec::new();
    for (source_index, page_uuid) in content_page_targets(&content_json) {
        let rm = match rm_map.get(page_uuid.as_str()) {
            Some(&sha) => Some(lib.read_blob(sha).map_err(|e| e.to_string())?),
            None => None,
        };
        // An inserted page with no ink has nothing to render and no backing
        // page to keep — skip it rather than emit an empty page.
        if source_index.is_none() && rm.is_none() {
            continue;
        }
        plan.push(rehydrate_render::PageSpec { source_index, rm });
    }
    Ok(plan)
}

/// Extract `(source_page_index, page_uuid)` entries from a parsed `.content`,
/// handling both reMarkable content-file layouts. `source_page_index` is
/// `Some(i)` for a page backed by PDF page `i`, or `None` for a notebook page
/// the user inserted between PDF pages (which has no backing PDF page).
///
/// * **formatVersion 2** (current devices, incl. reMarkable Paper Pro):
///   pages live under `cPages.pages[]` as *objects*, each with an `id` (the
///   page UUID) and, for PDF-backed pages, a `redir.value` (0-based backing
///   PDF page index). A missing `redir` marks an inserted notebook page.
/// * **formatVersion 1** (legacy): a top-level `pages[]` array of UUID
///   *strings*, positionally aligned with the PDF pages.
///
/// The old code understood neither the fV2 shape (so fV2 PDFs reported "no
/// annotations" and exported the bare source) nor inserted pages (which it
/// mapped onto the array index, stamping their ink on top of real PDF pages).
fn content_page_targets(content_json: &serde_json::Value) -> Vec<(Option<usize>, String)> {
    if let Some(arr) = content_json
        .pointer("/cPages/pages")
        .and_then(|v| v.as_array())
    {
        return arr
            .iter()
            .filter_map(|page| {
                let id = page.get("id")?.as_str()?.to_string();
                let redir = page
                    .get("redir")
                    .and_then(|r| r.get("value"))
                    .and_then(serde_json::Value::as_u64)
                    .map(|v| v as usize);
                Some((redir, id))
            })
            .collect();
    }

    if let Some(arr) = content_json.get("pages").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .enumerate()
            .filter_map(|(i, v)| Some((Some(i), v.as_str()?.to_string())))
            .collect();
    }

    vec![]
}

#[cfg(test)]
mod tests {
    use super::content_page_targets;

    #[test]
    fn parses_format_version_2_cpages() {
        // Shape taken verbatim from a real reMarkable Paper Pro PDF
        // `.content` (formatVersion 2): pages are objects under
        // `cPages.pages`, with the backing PDF page in `redir.value`.
        let json = serde_json::json!({
            "formatVersion": 2,
            "cPages": {
                "pages": [
                    { "id": "f02d67cd-4558-4d90-8b54-bfdc7ebe43cd",
                      "redir": { "timestamp": "2:2", "value": 0 } },
                    { "id": "2b91c8e2-6a8d-4c55-98c5-b332c9731e88",
                      "redir": { "timestamp": "2:2", "value": 1 } }
                ]
            }
        });
        assert_eq!(
            content_page_targets(&json),
            vec![
                (Some(0), "f02d67cd-4558-4d90-8b54-bfdc7ebe43cd".to_string()),
                (Some(1), "2b91c8e2-6a8d-4c55-98c5-b332c9731e88".to_string()),
            ]
        );
    }

    #[test]
    fn inserted_pages_have_no_source_index() {
        // Real "04_June2026 Roadmap" shape: two inserted note pages (no
        // `redir`) between PDF pages 0 and 1 → 18 notebook pages over a
        // 16-page PDF. Inserted pages must report `None`, not the array index.
        let json = serde_json::json!({
            "cPages": {
                "pages": [
                    { "id": "p0", "redir": { "value": 0 } },
                    { "id": "ins1" },
                    { "id": "ins2" },
                    { "id": "p1", "redir": { "value": 1 } },
                    { "id": "p2", "redir": { "value": 2 } }
                ]
            }
        });
        assert_eq!(
            content_page_targets(&json),
            vec![
                (Some(0), "p0".to_string()),
                (None, "ins1".to_string()),
                (None, "ins2".to_string()),
                (Some(1), "p1".to_string()),
                (Some(2), "p2".to_string()),
            ]
        );
    }

    #[test]
    fn parses_legacy_top_level_pages() {
        let json = serde_json::json!({ "pages": ["aaa", "bbb", "ccc"] });
        assert_eq!(
            content_page_targets(&json),
            vec![
                (Some(0), "aaa".to_string()),
                (Some(1), "bbb".to_string()),
                (Some(2), "ccc".to_string())
            ]
        );
    }

    #[test]
    fn empty_when_no_pages() {
        assert!(content_page_targets(&serde_json::json!({})).is_empty());
    }
}
