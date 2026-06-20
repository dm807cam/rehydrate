use std::path::PathBuf;

use rehydrate_core::{Manifest, VersionId};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

use super::transcribe::strip_frontmatter;
use super::TRANSCRIPT_PATH;
use crate::state::AppState;
use crate::util::{err, lib_arc};

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Txt,
    Markdown,
}

#[derive(Serialize)]
pub struct ExportTranscriptResult {
    pub path: PathBuf,
}

#[tauri::command]
pub async fn export_transcript(
    app: AppHandle,
    state: State<'_, AppState>,
    version_id: VersionId,
    format: ExportFormat,
) -> Result<Option<ExportTranscriptResult>, String> {
    let lib = lib_arc(&state).await?;
    let bytes = lib
        .read_derived_artefact(version_id, TRANSCRIPT_PATH)
        .map_err(err)?
        .ok_or_else(|| "no transcript on this version".to_string())?;
    let entry = lib.get_version(version_id).map_err(err)?;
    let manifest_bytes = lib.read_blob(&entry.manifest_hash).map_err(err)?;
    let manifest = Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;
    let safe_name = sanitize(&manifest.visible_name);
    let (default_name, ext) = match format {
        ExportFormat::Txt => (format!("{safe_name}-v{version_id}.txt"), "txt"),
        ExportFormat::Markdown => (format!("{safe_name}-v{version_id}.md"), "md"),
    };

    let app_for_pick = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_pick
            .dialog()
            .file()
            .add_filter(ext, &[ext])
            .set_file_name(&default_name)
            .blocking_save_file()
    })
    .await
    .map_err(err)?;

    let Some(file_path) = picked else {
        return Ok(None);
    };
    let target = file_path
        .into_path()
        .map_err(|e| format!("could not resolve picked file: {e}"))?;

    let payload: Vec<u8> = match format {
        ExportFormat::Markdown => bytes,
        ExportFormat::Txt => {
            // Strip frontmatter so the .txt is purely transcript text.
            let md = String::from_utf8_lossy(&bytes).into_owned();
            strip_frontmatter(&md).into_bytes()
        }
    };
    std::fs::write(&target, &payload).map_err(err)?;
    Ok(Some(ExportTranscriptResult { path: target }))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_string()
}
