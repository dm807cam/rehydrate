use rehydrate_core::{Manifest, VersionId};
use rehydrate_ocr::{
    OcrBackend, OcrError, OcrProgressEvent, OllamaBackend, TranscribeOptions,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::mpsc;

use super::ollama::{cached_reachable, record_ping};
use super::TRANSCRIPT_PATH;
use crate::config;
use crate::state::AppState;
use crate::util::{err, lib_arc};

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
    let ollama = config::load().ollama;

    // Reachability gate: if the cached probe says the daemon is
    // down, fail fast with a tagged error the renderer maps to
    // "open Settings → Ollama tab". A miss in the cache falls
    // through to the actual request, which surfaces the same error
    // shape via the backend.
    //
    // CRITICAL: the cache is *strictly* reachability now (see
    // `OllamaPing::reachable`). Earlier versions overloaded it
    // with "model present" or "OCR succeeded", which made
    // failure modes like "wrong model name" poison the cache as
    // "unreachable" for 30s and leave the user staring at an
    // error while Settings → Test connection happily said
    // everything was fine.
    if let Some(false) = cached_reachable(&state, &ollama.base_url).await {
        return Err(unconfigured_error(
            &ollama.base_url,
            &ollama.model,
            "cached probe failed",
        ));
    }

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
    // Memory guard: every page renders into a Vec<u8> PNG (~100 KB
    // per page typical, more for dense ink), and all PNGs live in
    // memory together while transcribe_pages iterates them serially
    // through Ollama. A 1 000-page notebook would peak around
    // 100 MB just for the PNG vec — fine on a desktop, but
    // pathological notebook sizes can balloon further. 500 pages
    // is a generous ceiling for any realistic notebook; users with
    // larger ones can split them, which is also a saner OCR
    // workflow (each split takes minutes on CPU).
    const MAX_OCR_PAGES: usize = 500;
    if rm_pages.len() > MAX_OCR_PAGES {
        return Err(format!(
            "notebook has {} pages — OCR is capped at {} pages to keep memory bounded. \
             Split the notebook on the tablet first.",
            rm_pages.len(),
            MAX_OCR_PAGES
        ));
    }

    // Render every page to PNG on a worker thread. Pages with no
    // ink are filtered out BEFORE the model is called — VLMs
    // (qwen3.5:4b in particular) confabulate plausible essay-shape
    // text when handed a pure-white canvas, and the user has hit
    // that bug ("blank notebook → multi-paragraph fake transcript
    // about AI in healthcare"). The blank indices are remembered
    // so the output markdown still preserves page ordering — each
    // skipped page becomes an empty entry in the same slot.
    let mut blobs = Vec::with_capacity(rm_pages.len());
    for f in &rm_pages {
        blobs.push(lib.read_blob(&f.sha256).map_err(err)?);
    }
    let total_pages = blobs.len();
    /// Bundle returned from the per-page rendering pass — keeps the
    /// `spawn_blocking` closure's signature out of clippy's
    /// type-complexity bucket and documents what each field is for
    /// at the call site.
    struct RenderedPages {
        /// PNG bytes for every non-blank page, in notebook order.
        pages_png: Vec<Vec<u8>>,
        /// One entry per notebook page: true if the page was blank
        /// and therefore skipped before the model was called.
        blank_mask: Vec<bool>,
        /// `slice_to_notebook[i]` = notebook index of the i-th
        /// element in `pages_png`. Used to remap backend progress
        /// events from slice-index back to notebook-index.
        slice_to_notebook: Vec<usize>,
    }
    let rendered =
        tauri::async_runtime::spawn_blocking(move || -> Result<RenderedPages, String> {
            let mut out = Vec::with_capacity(blobs.len());
            let mut blank = Vec::with_capacity(blobs.len());
            // Backend events index into the post-filter slice, but
            // the UI counts and reports against the notebook's real
            // page index. `slice_to_notebook[i]` lets the forwarder
            // remap a backend event's `page_index` back to the
            // user-visible position; without this the progress chip
            // ticks 1, 2, 3 for a 5-page notebook with two blanks
            // and the user thinks two pages went missing.
            let mut slice_to_notebook = Vec::new();
            for (idx, bytes) in blobs.iter().enumerate() {
                if !rehydrate_ocr::rm_page_has_ink(bytes) {
                    blank.push(true);
                    continue;
                }
                match rehydrate_ocr::render_rm_to_png(bytes) {
                    Ok(png) => {
                        out.push(png);
                        blank.push(false);
                        slice_to_notebook.push(idx);
                    }
                    Err(e) => {
                        return Err(format!("page {idx} render failed: {e}"));
                    }
                }
            }
            Ok(RenderedPages {
                pages_png: out,
                blank_mask: blank,
                slice_to_notebook,
            })
        })
        .await
        .map_err(err)??;
    let RenderedPages {
        pages_png,
        blank_mask,
        slice_to_notebook,
    } = rendered;
    let blank_count = blank_mask.iter().filter(|b| **b).count();
    if pages_png.is_empty() {
        // Every page in the notebook is blank. Returning an empty
        // transcript with a frontmatter header lets the renderer
        // show the "transcript exists but is empty" state instead
        // of failing with a confusing error — and it locks in the
        // page count so the user sees we did consider all N pages.
        let model_name = format!("ollama/{}", ollama.model);
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let markdown = format!(
            "---\nmodel: {model_name}\ncreated_at: {now}\nnote: skipped — all pages blank\n---\n\n"
        );
        let outcome = lib
            .record_derived_artefact(&document_id, TRANSCRIPT_PATH, markdown.as_bytes())
            .map_err(err)?;
        return Ok(TranscriptSummary {
            document_id,
            version_id: outcome.version_id,
            page_count: total_pages,
            char_count: 0,
            model: model_name,
        });
    }

    // Build the backend now that we know we have work to do. Cheap
    // (one AgentBuilder); fails fast on a bad URL so we surface the
    // tagged error before any PNG rendering.
    let backend = match OllamaBackend::new(&ollama.base_url, &ollama.model) {
        Ok(b) => b,
        Err(e) => {
            record_ping(&state, &ollama.base_url, false).await;
            return Err(unconfigured_error(
                &ollama.base_url,
                &ollama.model,
                &format!("{e}"),
            ));
        }
    };

    // Emit a `PageDone` event for every blank page upfront so the
    // progress chip ticks through the full notebook count rather
    // than stopping at the non-blank subset. The chars: 0 keeps the
    // running character total honest. Direct emit (rather than
    // routing through the channel) bypasses the remapper below,
    // which only applies to backend-originated events with a
    // slice-index.
    for (notebook_idx, was_blank) in blank_mask.iter().enumerate() {
        if *was_blank {
            let _ = app.emit(
                "ocr:progress",
                &OcrProgressEvent::PageDone {
                    page_index: notebook_idx,
                    chars: 0,
                },
            );
        }
    }

    // Forward backend progress to the renderer, remapping the
    // backend's slice-index `page_index` to the notebook's real
    // index so events emitted to the UI line up with the synthetic
    // blank events above.
    let (tx, mut rx) = mpsc::channel::<OcrProgressEvent>(32);
    let app_for_emit = app.clone();
    let forwarder = tauri::async_runtime::spawn(async move {
        while let Some(mut ev) = rx.recv().await {
            match &mut ev {
                OcrProgressEvent::PageStarted { page_index }
                | OcrProgressEvent::PageDone { page_index, .. }
                | OcrProgressEvent::PageFailed { page_index, .. } => {
                    if let Some(real) = slice_to_notebook.get(*page_index) {
                        *page_index = *real;
                    }
                }
                OcrProgressEvent::Done { .. } => {}
            }
            let _ = app_for_emit.emit("ocr:progress", &ev);
        }
    });

    let opts = TranscribeOptions {
        language,
        markdown: true,
    };
    // Reset the shared OCR cancel handle (it may have been tripped
    // by a previous `cancel_ocr` call); the renderer can flip it
    // again to abort the in-flight transcribe at the next page.
    state.ocr_cancel.reset();
    let pages_result = backend
        .transcribe_pages(pages_png, &opts, Some(tx), state.ocr_cancel.clone())
        .await;
    let _ = forwarder.await;
    let report = match pages_result {
        Ok(r) => {
            record_ping(&state, &ollama.base_url, true).await;
            r
        }
        Err(OcrError::Unreachable(msg)) => {
            record_ping(&state, &ollama.base_url, false).await;
            return Err(unconfigured_error(&ollama.base_url, &ollama.model, &msg));
        }
        Err(e) => {
            return Err(format!("OCR failed: {e}"));
        }
    };
    // Refuse to commit if every page failed. Without this guard the
    // user gets a "transcript saved" toast pointing at a file
    // containing nothing but failure placeholders — and worse, the
    // empty transcript would claim authority over the document
    // (next auto-OCR sweep would skip it because it "already has"
    // a transcript). Surface the per-page errors so support
    // diagnostics aren't a black box.
    if report.is_all_failed() {
        let sample = report
            .failures
            .first()
            .map(|f| f.message.clone())
            .unwrap_or_else(|| "(no failure details)".into());
        return Err(format!(
            "OCR failed for every page ({} pages attempted, none succeeded). \
             First failure: {sample}",
            report.failures.len()
        ));
    }
    let pages = report.pages;
    let page_failures = report.failures;

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
    // The frontmatter has to be honest about what's missing. The
    // user is going to act on this transcript (search, publish,
    // archive); if 3 of 50 pages failed they should see that at the
    // top of the file, not have it buried in an `ocr:progress`
    // event they never saw.
    let mut markdown = String::new();
    markdown.push_str("---\n");
    markdown.push_str(&format!("model: {model_name}\n"));
    markdown.push_str(&format!("created_at: {now}\n"));
    if !detected_lang.is_empty() {
        markdown.push_str(&format!("language: {detected_lang}\n"));
    }
    let transcribed_count = pages.len();
    markdown.push_str(&format!(
        "pages_transcribed: {transcribed_count} of {total_pages}\n"
    ));
    markdown.push_str("---\n\n");
    if blank_count > 0 {
        // Record how many pages we skipped so the user can spot it
        // in the transcript header without re-running OCR. Avoids
        // the previous failure mode where blank pages produced
        // hallucinated essay text indistinguishable from a real
        // transcript.
        markdown.push_str(&format!(
            "_note: {blank_count} blank page{} skipped_\n\n",
            if blank_count == 1 { "" } else { "s" }
        ));
    }
    if !page_failures.is_empty() {
        // Surface the failure count up front. Per-page placeholders
        // below make the gaps visible inline; this summary is for
        // skim-readers.
        markdown.push_str(&format!(
            "_note: {} page{} could not be transcribed (placeholder shown inline below)_\n\n",
            page_failures.len(),
            if page_failures.len() == 1 { "" } else { "s" }
        ));
    }
    let mut total_chars = 0usize;
    // Splice entries back together respecting the notebook's
    // original page order. Three classes:
    //   * blank → skip (already handled by blank_mask).
    //   * failed → emit a placeholder so the user can see WHERE the
    //     gap is and re-run OCR on those pages specifically.
    //   * succeeded → emit the transcribed text.
    // Pre-v1.0 we just iterated `pages.iter()` and dropped failed
    // pages silently — a wedged Ollama could produce a 1-page
    // transcript for a 200-page notebook with no signal.
    let failure_indices: std::collections::HashSet<usize> =
        page_failures.iter().map(|f| f.page_index).collect();
    let mut next_transcribed = pages.iter();
    for (i, was_blank) in blank_mask.iter().enumerate() {
        if i > 0 {
            markdown.push_str("\n\n");
        }
        if *was_blank {
            continue;
        }
        if failure_indices.contains(&i) {
            markdown.push_str(&format!(
                "*[Page {}: transcription failed — re-run OCR to retry]*",
                i + 1
            ));
            continue;
        }
        let Some(page) = next_transcribed.next() else {
            break;
        };
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
        // Reports the full notebook page count so the user sees a
        // truthful "X pages" tally regardless of how many were blank.
        page_count: total_pages,
        char_count: total_chars,
        model: model_name,
    })
}

/// Build the tagged error string the frontend uses to decide
/// whether to auto-open the Settings modal on the Ollama tab. We
/// JSON-encode rather than free-text so the renderer can match by
/// `.kind == "ollama_unconfigured"` and still see the human message.
fn unconfigured_error(base_url: &str, model: &str, reason: &str) -> String {
    let v = serde_json::json!({
        "kind": "ollama_unconfigured",
        "base_url": base_url,
        "model": model,
        "message": format!(
            "Couldn't reach Ollama at {base_url} (model {model}): {reason}"
        ),
    });
    v.to_string()
}

#[tauri::command]
pub async fn get_transcript(
    state: State<'_, AppState>,
    version_id: VersionId,
) -> Result<Option<TranscriptDocument>, String> {
    let lib = lib_arc(&state).await?;
    let bytes = match lib
        .read_derived_artefact(version_id, TRANSCRIPT_PATH)
        .map_err(err)?
    {
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

/// Drop the leading `---\n…---\n` block (if present) so the body
/// alone can be exported to .txt or sent to a publishing target as
/// HTML. Shared with `export_md` and `publish`; kept here next to
/// `parse_frontmatter` so the two views of the frontmatter live
/// together.
pub(super) fn strip_frontmatter(md: &str) -> String {
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

/// Live documents whose current version doesn't already have an
/// `ocr/transcript.md` derived artefact. The auto-OCR-at-startup
/// sweep iterates this list. Documents are returned in `(visible
/// name, doc id)` shape — the renderer wants the title for the
/// progress chip, the id for the actual `transcribe_document`
/// call.
#[derive(Serialize)]
pub struct OcrCandidate {
    pub document_id: String,
    pub visible_name: String,
}

#[tauri::command]
pub async fn list_documents_needing_ocr(
    state: State<'_, AppState>,
) -> Result<Vec<OcrCandidate>, String> {
    let lib = lib_arc(&state).await?;
    // `list_documents` is cheap (one SELECT + a manifest blob hash
    // per row); the per-doc `read_derived_artefact` is a manifest
    // parse + a hash lookup that returns None without reading any
    // additional blob bytes if the path isn't in the manifest.
    // O(docs); fine for libraries up to several thousand entries
    // and below the user's "wait, why is the app frozen" threshold.
    let docs = lib.list_documents().map_err(err)?;
    let mut out = Vec::with_capacity(docs.len());
    for doc in docs {
        // Notebooks are the only doc type the OCR pipeline can
        // handle today — PDFs and EPUBs carry their own text and
        // would just produce duplicate/inferior transcripts.
        if doc.doc_type != "Notebook" {
            continue;
        }
        let existing = lib
            .read_derived_artefact(doc.current_version_id, TRANSCRIPT_PATH)
            .map_err(err)?;
        if existing.is_none() {
            out.push(OcrCandidate {
                document_id: doc.document_id,
                visible_name: doc.visible_name,
            });
        }
    }
    Ok(out)
}
