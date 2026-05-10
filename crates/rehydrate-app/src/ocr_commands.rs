//! IPC commands for the OCR + publish pipeline. Lives in its own
//! module so `commands.rs` doesn't grow into a wall.
//!
//! Pattern matches the rest of the IPC layer:
//! - `lib_arc(&state)` for the open library.
//! - `tauri::async_runtime::spawn_blocking` for sync work (OCR
//!   inference, model download, ureq calls).
//! - Progress streamed via `app.emit("ocr:*", &event)`.
//! - Server-side dialog pickers for save targets so the renderer
//!   can't aim writes at arbitrary paths (audit fix H7 pattern).

use std::path::PathBuf;
use std::sync::Arc;

use rehydrate_core::{Library, Manifest, VersionId};
use rehydrate_ocr::{
    default_model_id, default_model_size_hint, OcrCancel, OcrProgressEvent, TranscribeOptions,
};
#[cfg(feature = "ocr-runtime")]
use rehydrate_ocr::OcrBackend;
use rehydrate_publish::{
    DraftPost, GhostClient, GhostCredentials, PublishResult, PublishTarget, Publisher,
    WordpressClient, WordpressCredentials,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::mpsc;

use crate::state::{
    AppState, KEYRING_GHOST_CREDS, KEYRING_SERVICE, KEYRING_WORDPRESS_CREDS,
};

const TRANSCRIPT_PATH: &str = "ocr/transcript.md";

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

async fn lib_arc(state: &State<'_, AppState>) -> Result<Arc<Library>, String> {
    state
        .library
        .lock()
        .await
        .as_ref()
        .map(Arc::clone)
        .ok_or_else(|| "no library is open".to_string())
}

// =====================================================================
//   Model lifecycle
// =====================================================================

/// Compact descriptor surfaced to the UI for the active default
/// model. The download URL / SHA / hf-hub layout is opaque to the
/// renderer — it only needs the human label and a size hint to
/// drive the onboarding text.
#[derive(Serialize, Clone)]
pub struct OcrModelDescriptor {
    pub id: String,
    pub display_name: String,
    pub size_bytes: u64,
}

fn current_descriptor() -> OcrModelDescriptor {
    OcrModelDescriptor {
        id: default_model_id().to_string(),
        display_name: format!("{} (4-bit ISQ)", default_model_id()),
        size_bytes: default_model_size_hint(),
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OcrStatusReport {
    /// Weights are not in the HF cache; the user has to start a
    /// multi-GB download.
    Missing { descriptor: OcrModelDescriptor },
    /// Weights are on disk but the backend isn't loaded into
    /// memory yet. The OCR dialog can skip the download prompt
    /// and go straight to a quick load step.
    Cached { descriptor: OcrModelDescriptor },
    /// Backend is loaded and can transcribe immediately.
    Ready { descriptor: OcrModelDescriptor },
}

#[tauri::command]
pub async fn ocr_status(state: State<'_, AppState>) -> Result<OcrStatusReport, String> {
    let descriptor = current_descriptor();
    // Three-way status. Active backend slot starts as `Mock` on
    // every app launch and is replaced with `MistralRsBackend`
    // once `ocr_download_default_model` finishes loading. So
    // "is the backend ready" is purely an in-process question.
    // "Has the user already downloaded the weights" needs a disk
    // check — without it, every relaunch looks like a fresh first
    // run and the user gets prompted to redownload 6 GB they
    // already have.
    let backend_loaded = state
        .ocr_backend
        .read()
        .await
        .name()
        .starts_with("mistralrs/");
    if backend_loaded {
        return Ok(OcrStatusReport::Ready { descriptor });
    }
    #[cfg(feature = "ocr-runtime")]
    if rehydrate_ocr::MistralRsBackend::is_cached(default_model_id()) {
        return Ok(OcrStatusReport::Cached { descriptor });
    }
    Ok(OcrStatusReport::Missing { descriptor })
}

/// Load (and download, if first run) the default vision-LLM and
/// install it as the active OCR backend. After this returns the
/// `transcribe_document` IPC will route through the real model
/// instead of the Mock placeholder.
///
/// Two-phase: (1) drive the HF download ourselves through hf-hub's
/// `download_with_progress`, streaming byte-level progress over
/// `ocr:progress`; (2) fire a `model_loading` event and hand off
/// to mistral.rs, which now hits the local cache and only pays
/// the mmap+ISQ cost. Splitting the phases is what gives the UI
/// something specific to render across the multi-minute setup —
/// the prior single-load call left the dialog on an indeterminate
/// spinner for the whole 60 minutes the user reported.
#[tauri::command]
pub async fn ocr_download_default_model(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    #[cfg(not(feature = "ocr-runtime"))]
    {
        let _ = (app, state);
        return Err(
            "this build was compiled without the OCR runtime — rebuild with --features ocr-runtime"
                .into(),
        );
    }

    #[cfg(feature = "ocr-runtime")]
    {
        let model_id = default_model_id().to_string();

        // No-op fast path: if a previous call in this process
        // already loaded the backend, there's nothing to do. The
        // dialog can call this repeatedly without re-loading.
        let already_loaded = state
            .ocr_backend
            .read()
            .await
            .name()
            .starts_with("mistralrs/");
        if already_loaded {
            let _ = app.emit("ocr:progress", &OcrProgressEvent::DownloadDone);
            return Ok(());
        }

        // Cached fast path: weights are on disk from a previous
        // session. Skip the download phase entirely, jump straight
        // to the load phase. This is the common path on every app
        // relaunch after the first successful download — without
        // it, the user gets re-prompted to download the same 6 GB
        // every launch.
        let cached = rehydrate_ocr::MistralRsBackend::is_cached(&model_id);

        if !cached {
            // Up-front "download started" event so the dialog leaves
            // the "missing" screen immediately, even before the first
            // byte lands.
            let _ = app.emit(
                "ocr:progress",
                &OcrProgressEvent::DownloadProgress { done: 0, total: None },
            );

            // Forward byte-level download progress from the blocking
            // hf-hub thread to the renderer.
            let (tx, mut rx) = mpsc::channel::<OcrProgressEvent>(32);
            let app_for_emit = app.clone();
            let forwarder = tauri::async_runtime::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    let _ = app_for_emit.emit("ocr:progress", &ev);
                }
            });

            let model_id_for_dl = model_id.clone();
            let dl_tx = tx.clone();
            let download = tauri::async_runtime::spawn_blocking(move || {
                rehydrate_ocr::MistralRsBackend::download_blocking(&model_id_for_dl, Some(dl_tx))
            });
            // Drop our copy of the sender so the forwarder exits when
            // the download task drops its.
            drop(tx);
            download
                .await
                .map_err(|e| format!("download task failed: {e}"))?
                .map_err(|e| format!("model download failed: {e}"))?;
            let _ = forwarder.await;
        }

        // Distinct phase: weights are on disk, but the runtime
        // still has to map the safetensors and apply ISQ-Q4 (AFQ4
        // on Metal, Q4K on CUDA). On a consumer M1 the ISQ pass
        // alone takes 1–3 minutes; without this event the dialog
        // has nothing to render across that window.
        let _ = app.emit("ocr:progress", &OcrProgressEvent::ModelLoading);

        let backend = rehydrate_ocr::MistralRsBackend::load(&model_id)
            .await
            .map_err(|e| format!("model load failed: {e}"))?;

        let arc: Arc<dyn OcrBackend> = Arc::new(backend);
        *state.ocr_backend.write().await = arc;

        let _ = app.emit("ocr:progress", &OcrProgressEvent::DownloadDone);
        Ok(())
    }
}

// =====================================================================
//   Transcribe
// =====================================================================

#[derive(Serialize)]
pub struct TranscriptSummary {
    pub document_id: String,
    pub version_id: VersionId,
    pub page_count: usize,
    pub char_count: usize,
    pub model: String,
}

#[derive(Serialize)]
pub struct TranscriptDocument {
    pub document_id: String,
    pub version_id: VersionId,
    pub markdown: String,
    pub model: Option<String>,
    pub created_at: Option<String>,
    pub language: Option<String>,
}

#[tauri::command]
pub async fn transcribe_document(
    app: AppHandle,
    state: State<'_, AppState>,
    document_id: String,
    language: Option<String>,
) -> Result<TranscriptSummary, String> {
    let lib = lib_arc(&state).await?;

    // Pull the manifest + every `.rm` page blob.
    let docs = lib.list_documents().map_err(err)?;
    let doc = docs
        .iter()
        .find(|d| d.document_id == document_id)
        .ok_or_else(|| format!("document {document_id} not in library"))?;
    let manifest_bytes = lib.read_blob(&doc.current_manifest).map_err(err)?;
    let manifest = Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;

    let mut rm_pages: Vec<_> = manifest
        .files
        .iter()
        .filter(|f| {
            !f.derived
                && f.path.ends_with(".rm")
                && !f.path.contains(".thumbnails")
                && !f.path.ends_with(".local")
        })
        .collect();
    rm_pages.sort_by(|a, b| a.path.cmp(&b.path));
    if rm_pages.is_empty() {
        return Err("document has no .rm pages to transcribe".into());
    }

    // Render every page to PNG on a worker thread so we don't block
    // the IPC runtime. Each page is independent, so we render in a
    // single spawn_blocking and pipe back.
    let mut blobs = Vec::with_capacity(rm_pages.len());
    for f in &rm_pages {
        blobs.push(lib.read_blob(&f.sha256).map_err(err)?);
    }
    let pages_png = tauri::async_runtime::spawn_blocking(move || {
        let mut out = Vec::with_capacity(blobs.len());
        for (idx, bytes) in blobs.iter().enumerate() {
            match rehydrate_ocr::render_rm_to_png(bytes) {
                Ok(png) => out.push(png),
                Err(e) => {
                    return Err(format!("page {idx} render failed: {e}"));
                }
            }
        }
        Ok::<Vec<Vec<u8>>, String>(out)
    })
    .await
    .map_err(err)??;

    // Forward backend progress to the renderer.
    let (tx, mut rx) = mpsc::channel::<OcrProgressEvent>(32);
    let app_for_emit = app.clone();
    let forwarder = tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let _ = app_for_emit.emit("ocr:progress", &ev);
        }
    });

    let backend = state.ocr_backend.read().await.clone();
    let opts = TranscribeOptions {
        language,
        markdown: true,
    };
    let cancel = OcrCancel::new();
    let pages = backend
        .transcribe_pages(pages_png, &opts, Some(tx), cancel)
        .await
        .map_err(|e| format!("OCR failed: {e}"))?;
    let _ = forwarder.await;

    // Assemble Markdown with a small frontmatter block recording
    // model + timestamp + language so future re-OCR can decide
    // whether to invalidate.
    let model_name = backend.name().to_string();
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let detected_lang = pages
        .iter()
        .find_map(|p| p.detected_language.clone())
        .unwrap_or_default();
    let mut markdown = String::new();
    markdown.push_str("---\n");
    markdown.push_str(&format!("model: {model_name}\n"));
    markdown.push_str(&format!("created_at: {now}\n"));
    if !detected_lang.is_empty() {
        markdown.push_str(&format!("language: {detected_lang}\n"));
    }
    markdown.push_str("---\n\n");
    let mut total_chars = 0usize;
    for (i, page) in pages.iter().enumerate() {
        if i > 0 {
            markdown.push_str("\n\n");
        }
        if !page.text.is_empty() {
            markdown.push_str(&page.text);
        }
        total_chars += page.text.chars().count();
    }
    if !markdown.ends_with('\n') {
        markdown.push('\n');
    }

    let outcome = lib
        .record_derived_artefact(&document_id, TRANSCRIPT_PATH, markdown.as_bytes())
        .map_err(err)?;

    Ok(TranscriptSummary {
        document_id,
        version_id: outcome.version_id,
        page_count: pages.len(),
        char_count: total_chars,
        model: model_name,
    })
}

#[tauri::command]
pub async fn get_transcript(
    state: State<'_, AppState>,
    version_id: VersionId,
) -> Result<Option<TranscriptDocument>, String> {
    let lib = lib_arc(&state).await?;
    let bytes = match lib.read_derived_artefact(version_id, TRANSCRIPT_PATH).map_err(err)? {
        Some(b) => b,
        None => return Ok(None),
    };
    let entry = lib.get_version(version_id).map_err(err)?;
    let markdown = String::from_utf8_lossy(&bytes).into_owned();
    let (model, created_at, language) = parse_frontmatter(&markdown);
    Ok(Some(TranscriptDocument {
        document_id: entry.document_id,
        version_id,
        markdown,
        model,
        created_at,
        language,
    }))
}

fn parse_frontmatter(md: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut model = None;
    let mut created_at = None;
    let mut language = None;
    let trimmed = md.strip_prefix("---\n").unwrap_or(md);
    if trimmed.as_ptr() == md.as_ptr() {
        return (model, created_at, language);
    }
    if let Some(end) = trimmed.find("\n---") {
        for line in trimmed[..end].lines() {
            if let Some(v) = line.strip_prefix("model: ") {
                model = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("created_at: ") {
                created_at = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("language: ") {
                language = Some(v.trim().to_string());
            }
        }
    }
    (model, created_at, language)
}

// =====================================================================
//   Export transcript
// =====================================================================

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
        .map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_') { c } else { '_' })
        .collect::<String>()
        .trim()
        .to_string()
}

fn strip_frontmatter(md: &str) -> String {
    let trimmed = md.strip_prefix("---\n").unwrap_or(md);
    if trimmed.as_ptr() == md.as_ptr() {
        return md.to_string();
    }
    if let Some(end) = trimmed.find("\n---") {
        let after = &trimmed[end + 4..];
        return after.trim_start_matches('\n').to_string();
    }
    md.to_string()
}

// =====================================================================
//   Publish
// =====================================================================

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublishKind {
    Ghost,
    Wordpress,
}

impl PublishKind {
    fn target(&self) -> PublishTarget {
        match self {
            PublishKind::Ghost => PublishTarget::Ghost,
            PublishKind::Wordpress => PublishTarget::Wordpress,
        }
    }
}

#[tauri::command]
pub async fn publish_transcript(
    state: State<'_, AppState>,
    version_id: VersionId,
    target: PublishKind,
) -> Result<PublishResult, String> {
    let lib = lib_arc(&state).await?;
    let bytes = lib
        .read_derived_artefact(version_id, TRANSCRIPT_PATH)
        .map_err(err)?
        .ok_or_else(|| "no transcript on this version".to_string())?;
    let md = String::from_utf8_lossy(&bytes).into_owned();
    let body = strip_frontmatter(&md);
    let mut html = String::new();
    pulldown_cmark::html::push_html(
        &mut html,
        pulldown_cmark::Parser::new(&body),
    );

    let entry = lib.get_version(version_id).map_err(err)?;
    let manifest_bytes = lib.read_blob(&entry.manifest_hash).map_err(err)?;
    let manifest = Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;

    let post = DraftPost {
        title: manifest.visible_name.clone(),
        html,
        tags: vec!["from-rehydrate".into()],
    };

    let kind = target.target();
    tauri::async_runtime::spawn_blocking(move || -> Result<PublishResult, String> {
        let publisher: Box<dyn Publisher> = match kind {
            PublishTarget::Ghost => Box::new(load_ghost_client()?),
            PublishTarget::Wordpress => Box::new(load_wordpress_client()?),
        };
        publisher.publish_draft(&post).map_err(err)
    })
    .await
    .map_err(err)?
}

fn load_ghost_client() -> Result<GhostClient, String> {
    let json = read_keychain(KEYRING_GHOST_CREDS)
        .ok_or_else(|| "no Ghost credentials saved".to_string())?;
    let creds: GhostCredentials = serde_json::from_str(&json).map_err(err)?;
    GhostClient::new(creds).map_err(err)
}

fn load_wordpress_client() -> Result<WordpressClient, String> {
    let json = read_keychain(KEYRING_WORDPRESS_CREDS)
        .ok_or_else(|| "no WordPress credentials saved".to_string())?;
    let creds: WordpressCredentials = serde_json::from_str(&json).map_err(err)?;
    WordpressClient::new(creds).map_err(err)
}

fn read_keychain(slot: &str) -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, slot).ok()?;
    entry.get_password().ok()
}

fn write_keychain(slot: &str, value: &str) -> Result<(), String> {
    keyring::Entry::new(KEYRING_SERVICE, slot)
        .map_err(err)?
        .set_password(value)
        .map_err(err)
}

fn forget_keychain(slot: &str) -> Result<(), String> {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, slot) {
        // Some keyring backends return a "not found" error when the
        // entry doesn't exist; treat as success.
        let _ = entry.delete_credential();
    }
    Ok(())
}

#[tauri::command]
pub async fn set_ghost_credentials(creds: GhostCredentials) -> Result<(), String> {
    let json = serde_json::to_string(&creds).map_err(err)?;
    write_keychain(KEYRING_GHOST_CREDS, &json)
}

#[tauri::command]
pub async fn forget_ghost_credentials() -> Result<(), String> {
    forget_keychain(KEYRING_GHOST_CREDS)
}

#[tauri::command]
pub async fn set_wordpress_credentials(creds: WordpressCredentials) -> Result<(), String> {
    let json = serde_json::to_string(&creds).map_err(err)?;
    write_keychain(KEYRING_WORDPRESS_CREDS, &json)
}

#[tauri::command]
pub async fn forget_wordpress_credentials() -> Result<(), String> {
    forget_keychain(KEYRING_WORDPRESS_CREDS)
}

#[derive(Serialize)]
pub struct PublishCredentialStatus {
    pub ghost: bool,
    pub wordpress: bool,
}

#[tauri::command]
pub async fn publish_credential_status() -> Result<PublishCredentialStatus, String> {
    Ok(PublishCredentialStatus {
        ghost: read_keychain(KEYRING_GHOST_CREDS).is_some(),
        wordpress: read_keychain(KEYRING_WORDPRESS_CREDS).is_some(),
    })
}

#[tauri::command]
pub async fn ping_publish_target(target: PublishKind) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let publisher: Box<dyn Publisher> = match target.target() {
            PublishTarget::Ghost => Box::new(load_ghost_client()?),
            PublishTarget::Wordpress => Box::new(load_wordpress_client()?),
        };
        publisher.ping().map_err(err)
    })
    .await
    .map_err(err)?
}
