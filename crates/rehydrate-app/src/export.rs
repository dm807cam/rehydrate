use std::collections::HashMap;
use std::path::PathBuf;

use rehydrate_core::Library;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;
use crate::util::{err, lib_arc};

#[derive(Serialize, Deserialize, Clone)]
struct ExportStateEntry {
    version_id: i64,
    path: String,
    #[serde(default)]
    include_annotations: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct ExportState {
    documents: HashMap<String, ExportStateEntry>,
}

#[derive(Serialize, Clone)]
pub struct ExportProgress {
    pub current: usize,
    pub total: usize,
    pub current_file: String,
}

#[derive(Serialize)]
pub struct ExportAsPdfsResult {
    pub exported: usize,
    pub skipped: usize,
    pub deleted: usize,
    pub target_dir: PathBuf,
}

const EXPORT_STATE_FILENAME: &str = ".rehydrate-export.json";

fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c == '/' || c == '\\' || c == ':' {
            out.push('-');
        } else {
            out.push(c);
        }
    }
    let trimmed = out.trim_matches(|c: char| c == '.' || c == ' ');
    if trimmed.is_empty() {
        "Untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Helper to build a mapping from folder_id to its full relative path.
fn build_folder_paths(
    folders: &[rehydrate_core::FolderEntry],
) -> HashMap<String, String> {
    let mut paths = HashMap::new();
    let mut parents: HashMap<String, Option<String>> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();

    for f in folders {
        parents.insert(f.folder_id.clone(), f.parent.clone());
        names.insert(f.folder_id.clone(), sanitize_filename(&f.visible_name));
    }

    for f in folders {
        let mut path_parts = Vec::new();
        let mut current_id = Some(f.folder_id.clone());

        while let Some(id) = current_id {
            if let Some(name) = names.get(&id) {
                path_parts.push(name.clone());
            }
            current_id = parents.get(&id).cloned().flatten();
        }

        path_parts.reverse();
        paths.insert(f.folder_id.clone(), path_parts.join("/"));
    }

    paths
}

/// Render a single document to PDF/EPUB bytes, applying annotation overlay
/// if requested. Shared by `export_as_pdfs` and `export_selected_as_pdfs`.
fn render_document_bytes(
    doc_id: &str,
    visible_name: &str,
    manifest: &rehydrate_core::Manifest,
    include_annotations: bool,
    lib: &Library,
) -> Result<Vec<u8>, String> {
    let body = manifest
        .files
        .iter()
        .find(|f| f.path.ends_with(".pdf") || f.path.ends_with(".epub"));

    if let Some(body) = body {
        let mut body_bytes = lib.read_blob(&body.sha256).map_err(err)?;
        if include_annotations && body.path.ends_with(".pdf") {
            if let Ok(plan) = crate::commands::pdf_annotation_plan(manifest, doc_id, lib) {
                if !plan.is_empty() {
                    match rehydrate_render::assemble_annotated_pdf(&body_bytes, &plan) {
                        Ok(assembled) => body_bytes = assembled,
                        Err(e) => {
                            tracing::warn!("export overlay failed for {doc_id}: {e}")
                        }
                    }
                }
            }
        }
        Ok(body_bytes)
    } else {
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
        if !rm_pages.is_empty() {
            let mut bufs = Vec::with_capacity(rm_pages.len());
            for f in &rm_pages {
                bufs.push(lib.read_blob(&f.sha256).map_err(err)?);
            }
            match rehydrate_render::build_pdf_from_rm_files(visible_name, &bufs) {
                Ok(b) => Ok(b),
                Err(e) => {
                    tracing::warn!("export rendering failed for {doc_id}: {e}");
                    build_thumbnail_fallback(lib, manifest, visible_name)
                }
            }
        } else {
            build_thumbnail_fallback(lib, manifest, visible_name)
        }
    }
}

fn build_thumbnail_fallback(
    lib: &Library,
    manifest: &rehydrate_core::Manifest,
    title: &str,
) -> Result<Vec<u8>, String> {
    let mut thumbs: Vec<_> = manifest
        .files
        .iter()
        .filter(|f| f.path.ends_with(".png") && f.path.contains(".thumbnails"))
        .collect();
    if thumbs.is_empty() {
        return Err(
            "notebook has no .rm ink files we can parse and no thumbnails to fall back on"
                .to_string(),
        );
    }
    thumbs.sort_by(|a, b| a.path.cmp(&b.path));
    let mut pages = Vec::with_capacity(thumbs.len());
    for f in &thumbs {
        pages.push(lib.read_blob(&f.sha256).map_err(err)?);
    }
    rehydrate_render::build_pdf_from_pngs(title, &pages)
}

#[tauri::command]
pub async fn export_as_pdfs(
    target_dir: String,
    root_folder_id: Option<String>,
    include_annotations: bool,
    keep_deleted: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ExportAsPdfsResult, String> {
    let lib = lib_arc(&state).await?;
    let target_path = PathBuf::from(&target_dir);

    // Read previous export state
    let state_file_path = target_path.join(EXPORT_STATE_FILENAME);
    let mut export_state: ExportState = if state_file_path.exists() {
        let content = std::fs::read_to_string(&state_file_path).map_err(err)?;
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        ExportState::default()
    };

    let folders = lib.list_folders().map_err(err)?;
    let documents = lib.list_documents().map_err(err)?;

    let folder_paths = build_folder_paths(&folders);

    // Determine the root prefix we are exporting, if any.
    let root_prefix = if let Some(ref rid) = root_folder_id {
        folder_paths.get(rid).map(|path| format!("{path}/"))
    } else {
        None
    };

    let mut to_export = Vec::new();
    let mut current_document_paths = HashMap::new();

    for doc in documents {
        // Skip trash
        if doc.parent.as_deref() == Some("trash") {
            continue;
        }

        let mut folder_path = String::new();
        if let Some(pid) = &doc.parent {
            if let Some(p) = folder_paths.get(pid) {
                folder_path = p.clone();
            }
        }

        let is_under_root = match &root_prefix {
            Some(prefix) => {
                if folder_path == prefix.trim_end_matches('/') || folder_path.starts_with(prefix) {
                    // Strip the root prefix from the relative path
                    let stripped = folder_path.strip_prefix(prefix).unwrap_or(&folder_path);
                    folder_path = stripped.to_string();
                    true
                } else {
                    false
                }
            }
            None => true,
        };

        if !is_under_root {
            continue;
        }

        let ext = if doc.doc_type == "DocumentType.Epub" {
            "epub"
        } else {
            "pdf"
        };

        let suffix = format!(".{ext}");
        let base_name = if doc.visible_name.to_ascii_lowercase().ends_with(&suffix) {
            &doc.visible_name[..doc.visible_name.len() - suffix.len()]
        } else {
            &doc.visible_name
        };
        let safe_name = sanitize_filename(base_name);

        let rel_path = if folder_path.is_empty() {
            format!("{safe_name}.{ext}")
        } else {
            format!("{folder_path}/{safe_name}.{ext}")
        };

        current_document_paths.insert(doc.document_id.clone(), rel_path.clone());
        to_export.push((doc, rel_path));
    }

    let total = to_export.len();
    let mut exported = 0;
    let mut skipped = 0;
    let mut deleted = 0;

    for (i, (doc, rel_path)) in to_export.into_iter().enumerate() {
        let _ = app.emit("export:progress", ExportProgress {
            current: i + 1,
            total,
            current_file: doc.visible_name.clone(),
        });

        let target_file = target_path.join(&rel_path);

        let mut needs_export = true;
        if let Some(prev) = export_state.documents.get(&doc.document_id) {
            if prev.version_id == doc.current_version_id
                && target_file.exists()
                && prev.path == rel_path
                && prev.include_annotations == include_annotations
            {
                needs_export = false;
            }

            // If path changed, remove the old file
            if prev.path != rel_path {
                let old_file = target_path.join(&prev.path);
                if old_file.exists() {
                    let _ = std::fs::remove_file(old_file);
                    deleted += 1;
                }
            }
        }

        if needs_export {
            if let Some(parent) = target_file.parent() {
                std::fs::create_dir_all(parent).map_err(err)?;
            }

            let manifest_bytes = lib.read_blob(&doc.current_manifest).map_err(err)?;
            let manifest =
                rehydrate_core::Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;

            let bytes = render_document_bytes(
                &doc.document_id,
                &doc.visible_name,
                &manifest,
                include_annotations,
                &lib,
            )?;

            std::fs::write(&target_file, bytes).map_err(err)?;
            exported += 1;

            export_state.documents.insert(
                doc.document_id.clone(),
                ExportStateEntry {
                    version_id: doc.current_version_id,
                    path: rel_path.clone(),
                    include_annotations,
                },
            );
        } else {
            skipped += 1;
        }
    }

    // Clean up documents that were removed or moved to trash or out of scope.
    // When keep_deleted is true the local file is preserved (backup mode);
    // either way the entry is dropped from the state file so future runs
    // don't re-examine documents that are no longer in the library.
    let mut to_remove = Vec::new();
    for (doc_id, prev) in &export_state.documents {
        if !current_document_paths.contains_key(doc_id) {
            if !keep_deleted {
                let old_file = target_path.join(&prev.path);
                if old_file.exists() {
                    let _ = std::fs::remove_file(old_file);
                    deleted += 1;
                }
            }
            to_remove.push(doc_id.clone());
        }
    }

    for id in to_remove {
        export_state.documents.remove(&id);
    }

    // Save state
    let serialized_state = serde_json::to_string_pretty(&export_state).map_err(err)?;
    std::fs::write(&state_file_path, serialized_state).map_err(err)?;

    Ok(ExportAsPdfsResult {
        exported,
        skipped,
        deleted,
        target_dir: target_path,
    })
}

/// Export a specific set of documents (by ID) to a folder, preserving the
/// library folder structure. No state file is used — every call re-exports
/// documents that have changed, making this suitable for ad-hoc selections.
#[tauri::command]
pub async fn export_selected_as_pdfs(
    target_dir: String,
    document_ids: Vec<String>,
    include_annotations: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ExportAsPdfsResult, String> {
    let lib = lib_arc(&state).await?;
    let target_path = PathBuf::from(&target_dir);

    let id_set: std::collections::HashSet<&str> =
        document_ids.iter().map(String::as_str).collect();

    let folders = lib.list_folders().map_err(err)?;
    let documents = lib.list_documents().map_err(err)?;
    let folder_paths = build_folder_paths(&folders);

    let to_export: Vec<_> = documents
        .into_iter()
        .filter(|d| id_set.contains(d.document_id.as_str()))
        .collect();

    let total = to_export.len();
    let mut exported = 0;

    for (i, doc) in to_export.into_iter().enumerate() {
        let _ = app.emit("export:progress", ExportProgress {
            current: i + 1,
            total,
            current_file: doc.visible_name.clone(),
        });

        let ext = if doc.doc_type == "DocumentType.Epub" { "epub" } else { "pdf" };
        let suffix = format!(".{ext}");
        let base_name = if doc.visible_name.to_ascii_lowercase().ends_with(&suffix) {
            &doc.visible_name[..doc.visible_name.len() - suffix.len()]
        } else {
            &doc.visible_name
        };
        let safe_name = sanitize_filename(base_name);

        let folder_path = doc
            .parent
            .as_deref()
            .and_then(|pid| folder_paths.get(pid))
            .cloned()
            .unwrap_or_default();

        let rel_path = if folder_path.is_empty() {
            format!("{safe_name}.{ext}")
        } else {
            format!("{folder_path}/{safe_name}.{ext}")
        };

        let target_file = target_path.join(&rel_path);
        if let Some(parent) = target_file.parent() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }

        let manifest_bytes = lib.read_blob(&doc.current_manifest).map_err(err)?;
        let manifest =
            rehydrate_core::Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;

        let bytes = render_document_bytes(
            &doc.document_id,
            &doc.visible_name,
            &manifest,
            include_annotations,
            &lib,
        )?;

        std::fs::write(&target_file, bytes).map_err(err)?;
        exported += 1;
    }

    Ok(ExportAsPdfsResult {
        exported,
        skipped: 0,
        deleted: 0,
        target_dir: target_path,
    })
}
