//! Embedded vision-LLM backend backed by mistral.rs.
//!
//! Compiled only when the `mistral` feature is enabled. Without
//! it, the crate ships only the `Mock` backend and builds in a
//! fraction of the time (mistral.rs pulls candle + Metal kernels
//! and adds 5–10 minutes to a cold build).
//!
//! Privacy: model weights are fetched by `hf-hub`, which only
//! contacts `huggingface.co`. Triggered exclusively by the user
//! clicking "Download model" — never on launch. The `no_egress`
//! integration test waives the reqwest ban for `rehydrate-ocr`
//! because hf-hub depends on it; we still ban it in the
//! business-logic crates (core / device / sync).
//!
//! Quality: defaults to Qwen2.5-VL-3B-Instruct (unquantized base)
//! plus runtime ISQ-Q4. On Metal, `with_auto_isq(IsqBits::Four)`
//! selects AFQ4 (Apple-format quantization) at load time; on CUDA
//! it selects Q4K. ~6 GB one-time download + a 1–2 minute
//! load-time ISQ pass on M1; ~2 GB resident afterwards.
//!
//! We tried two smaller-download paths and both hit walls in
//! mistral.rs 0.8.1 — leaving notes here so a future contributor
//! doesn't repeat the experiment:
//!
//!   * `Qwen/Qwen2.5-VL-3B-Instruct-AWQ` (3.4 GB pre-quantized):
//!     mistral.rs's GPTQ/AWQ kernels are CUDA-only
//!     (`mistralrs-quant-0.8.1/src/gptq/gptq_cpu.rs:18` bails
//!     with "GPTQ is only supported on CUDA"). Unusable on Metal.
//!   * Pre-built UQFF AFQ4 bundle (~3 GB):
//!     `Qwen2_5VLModel::residual_tensors`
//!     (`mistralrs-core-0.8.1/src/vision_models/qwen2_5_vl/
//!     mod.rs:639`) returns only the language tower's residuals
//!     and never serialises the vision encoder. Loading the
//!     resulting UQFF then fails with "cannot find tensor
//!     visual.blocks.0.norm1.weight". Upstream bug.
//!
//! Revisit either path once mistral.rs ships Metal AWQ kernels
//! or fixes the UQFF residual coverage for VLMs.

#![cfg(feature = "mistral")]

use std::sync::Arc;

use async_trait::async_trait;
use hf_hub::api::sync::{ApiBuilder, ApiError};
use hf_hub::api::Progress as HfProgress;
use hf_hub::Cache;
use mistralrs::{IsqBits, Model, MultimodalModelBuilder, TextMessageRole};
use tokio::sync::mpsc;

use crate::backend::{OcrBackend, OcrCancel, OcrError, PageTranscript, TranscribeOptions};
use crate::progress::OcrProgressEvent;

/// HuggingFace repo ID for the default VLM. Unquantized
/// Qwen2.5-VL-3B-Instruct (~6 GB safetensors split across two
/// shards) — the runtime applies AFQ4 at load time on Metal and
/// Q4K on CUDA via `with_auto_isq`.
pub const DEFAULT_MODEL_ID: &str = "Qwen/Qwen2.5-VL-3B-Instruct";

/// Cap on the longest image edge handed to the VLM. Held at 1280
/// because mistral.rs's Qwen2.5-VL m-RoPE path errored at 1568 on
/// Metal (`index-select invalid index 1574 with dim size 1574`,
/// `vision.rs:395`). Keep in sync with
/// `page_render::TARGET_LONG_EDGE`, which pre-resizes to this same
/// value — this cap is then a belt-and-braces clamp for any image
/// that slipped through without the standard render path.
const VLM_MAX_EDGE: u32 = 1280;

/// Files we don't bother downloading. The HF repo includes a few
/// large redundant fp32/onnx mirrors and assorted release noise
/// that the runtime never opens — pulling them costs the user
/// gigabytes of unnecessary bandwidth on first launch.
fn skip_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with(".git")
        || lower.ends_with(".gif")
        || lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".md")
        || lower == "readme"
    {
        return true;
    }
    // Skip pickle / consolidated / onnx mirrors when safetensors
    // are present in the same repo (they always are for Qwen2.5-VL).
    if lower.ends_with(".bin")
        || lower.ends_with(".pt")
        || lower.ends_with(".pth")
        || lower.ends_with(".onnx")
        || lower.ends_with(".onnx_data")
        || lower.contains("consolidated.")
    {
        return true;
    }
    false
}

/// Prompt handed to the VLM along with each page raster. Tuned
/// for the Qwen2.5-VL handwriting failure modes we see most:
///   * silently dropping unclear words instead of guessing,
///   * "summarising" rather than transcribing on long passages,
///   * re-formatting maths or code into prose,
///   * appending "I see a notebook page that says…" preambles.
/// Each line of guidance below targets one of those.
const TRANSCRIBE_PROMPT_BASE: &str = "Transcribe the handwritten text in this image verbatim — \
exact words, exact spelling, exact punctuation, exact line breaks. \
Preserve paragraph structure, lists, headings, indentation, and any underlining or emphasis you can see. \
Reproduce mathematical expressions, equations, and code samples character-for-character; do not paraphrase or convert them. \
If a word is unclear, transcribe your single best guess rather than skipping it. \
If a region is genuinely illegible, write [illegible] in its place — do not omit it silently. \
Output the transcript only — no preamble, no commentary, no description of the image, no markdown code fences.";

pub struct MistralRsBackend {
    name: String,
    model: Arc<Model>,
}

impl MistralRsBackend {
    /// Cheap, network-free check: does the local HF cache hold the
    /// weight files this backend needs to load `model_id`? Used by
    /// the IPC layer's `ocr_status` so a relaunch after a
    /// successful download skips the "Missing" prompt and goes
    /// straight to lazy load.
    ///
    /// We intentionally check only the safetensors shards + the
    /// shard index — those are the bulk of the bytes and the part
    /// the loader can't synthesise. If they're cached the small
    /// config files (config.json / tokenizer.json / etc.) almost
    /// certainly are too, since they're tiny and download first.
    /// `download_blocking`'s end-of-loop verification still runs
    /// before any actual load, so a freak partial cache won't
    /// silently produce a broken transcript.
    ///
    /// Hardcodes the Qwen2.5-VL-3B-Instruct shard layout. When
    /// the default model changes, update this list.
    pub fn is_cached(model_id: &str) -> bool {
        let cache = Cache::from_env();
        let cache_repo = cache.model(model_id.to_string());
        const REQUIRED: &[&str] = &[
            "config.json",
            "tokenizer.json",
            "model.safetensors.index.json",
            "model-00001-of-00002.safetensors",
            "model-00002-of-00002.safetensors",
        ];
        REQUIRED.iter().all(|f| cache_repo.get(f).is_some())
    }

    /// Pre-download every file the runtime will touch, with
    /// byte-level progress streamed to `progress`. After this
    /// returns, the HF cache is populated and [`Self::load`] hits
    /// the local files without making more HTTP calls.
    ///
    /// Splitting download from load lets the UI:
    ///   1. show real GB/s + ETA during the 60-minute first run,
    ///   2. surface a distinct "loading model into memory" phase
    ///      for the multi-minute mmap+ISQ step that follows,
    /// neither of which mistral.rs's bundled hf-hub progress (which
    /// only goes to stderr's indicatif bar) can do.
    ///
    /// Works equally well for the unquantized base or a UQFF
    /// bundle — we just iterate the repo's `siblings` and grab
    /// what the skip-list doesn't filter out.
    pub fn download_blocking(
        model_id: &str,
        progress: Option<mpsc::Sender<OcrProgressEvent>>,
    ) -> Result<(), OcrError> {
        let api = ApiBuilder::new()
            // We're driving our own progress, so suppress hf-hub's
            // stderr indicatif bar.
            .with_progress(false)
            .build()
            .map_err(map_hf_err)?;
        let repo = api.model(model_id.to_string());

        let info = repo.info().map_err(map_hf_err)?;

        // Filter the sibling list down to what we actually need so
        // the user doesn't pay for ONNX mirrors / READMEs / etc.
        let wanted: Vec<String> = info
            .siblings
            .iter()
            .map(|s| s.rfilename.clone())
            .filter(|n| !skip_file(n))
            .collect();

        if wanted.is_empty() {
            return Err(OcrError::Inference(format!(
                "no downloadable files found in {model_id}",
            )));
        }

        // For each wanted file, check the local cache first. hf-hub's
        // `download_with_progress` does NOT short-circuit on cache
        // hits — it always downloads to a fresh `.partial` file and
        // renames it over the existing blob at the end. So calling it
        // unconditionally re-downloads gigabytes on every launch
        // even though the bytes are already on disk. The cheaper
        // `repo.get` checks the snapshot symlink first, but doesn't
        // accept a Progress callback. We do it ourselves: ask the
        // cache (via `Cache::from_env`, so HF_HOME is honoured),
        // and only fall through to `download_with_progress` for
        // files that aren't already cached. Cache hits emit a
        // single progress event covering the file's full size so
        // the bar advances and the UI doesn't think we're stuck.
        let cache = Cache::from_env();
        let cache_repo = cache.model(model_id.to_string());

        let aggregator = Arc::new(std::sync::Mutex::new(ProgressAggState {
            total_bytes: 0,
            done_bytes: 0,
            sender: progress.clone(),
        }));

        for filename in &wanted {
            if let Some(cached_path) = cache_repo.get(filename) {
                // File is fully cached. Bump aggregate progress by
                // the file's on-disk size so the user sees forward
                // motion even on an entirely-cached relaunch.
                let size = std::fs::metadata(&cached_path).map(|m| m.len()).unwrap_or(0);
                let mut s = aggregator.lock().expect("progress mutex poisoned");
                s.total_bytes = s.total_bytes.saturating_add(size);
                s.done_bytes = s.done_bytes.saturating_add(size);
                emit(&s);
            } else {
                let cb = ProgressCallback::new(Arc::clone(&aggregator));
                repo.download_with_progress(filename, cb)
                    .map_err(map_hf_err)?;
            }
        }

        // Verification pass. Catches the case we hit during early
        // testing where a previous interrupted download (or a
        // transient hf-hub failure that returned Ok-with-missing-data)
        // left the cache half-populated, and the next `load()` call
        // failed deep inside mistral.rs with an opaque "cannot find
        // tensor X" message. Specifically, the snapshot dir was
        // missing `model.safetensors.index.json` even though every
        // `download_with_progress` call had returned `Ok`; the next
        // `load` then read shard 1 only, couldn't find the
        // vision-encoder tensors, and bailed.
        //
        // `cache_repo.get` checks the snapshot symlink (not just the
        // blob), so this confirms each wanted file actually landed.
        let mut missing: Vec<String> = Vec::new();
        for filename in &wanted {
            if cache_repo.get(filename).is_none() {
                missing.push(filename.clone());
            }
        }
        if !missing.is_empty() {
            return Err(OcrError::Inference(format!(
                "download finished but {} file(s) missing from cache: {}. \
                 The HF cache for this model may be corrupt — clear it with \
                 `rm -rf ~/.cache/huggingface/hub/models--{}` and retry.",
                missing.len(),
                missing.join(", "),
                model_id.replace('/', "--"),
            )));
        }

        Ok(())
    }

    /// Build a vision-LLM model handle from the local HF cache.
    /// Assumes [`Self::download_blocking`] has already populated
    /// the cache; if it hasn't, mistral.rs will fall back to
    /// downloading what's missing on its own (silently).
    ///
    /// Runs ISQ-Q4 at load time — AFQ4 on Metal, Q4K on CUDA. Peak
    /// load-time RAM is ~6 GB on M1 as the bf16 weights are read
    /// in and re-quantized chunk by chunk to ~2 GB resident.
    pub async fn load(model_id: impl Into<String>) -> Result<Self, OcrError> {
        let id = model_id.into();
        tracing::info!(model = %id, "loading mistral.rs vision model");
        let model = MultimodalModelBuilder::new(&id)
            .with_auto_isq(IsqBits::Four)
            .with_max_edge(VLM_MAX_EDGE)
            .with_logging()
            .build()
            .await
            .map_err(|e| OcrError::Inference(format!("model load failed: {e}")))?;
        tracing::info!(model = %id, "mistral.rs model ready");
        Ok(Self {
            name: format!("mistralrs/{id}"),
            model: Arc::new(model),
        })
    }
}

fn map_hf_err(e: ApiError) -> OcrError {
    OcrError::Inference(format!("hf-hub: {e}"))
}

/// Shared state across per-file `Progress` callbacks so the
/// progress channel reports cumulative bytes across the whole
/// snapshot, not just the in-flight file.
struct ProgressAggState {
    total_bytes: u64,
    done_bytes: u64,
    sender: Option<mpsc::Sender<OcrProgressEvent>>,
}

/// Per-file progress sink. Tracks the bytes this callback has
/// already contributed to the global aggregate so that hf-hub's
/// implementation quirks (it calls `init` twice — once in
/// `download_tempfile`, once in `download_from` — and again on
/// retry, plus an initial `update(start)` for the resume offset)
/// translate into a monotonic global tally.
struct ProgressCallback {
    state: Arc<std::sync::Mutex<ProgressAggState>>,
    file_total_contributed: u64,
    file_done_contributed: u64,
}

impl ProgressCallback {
    fn new(state: Arc<std::sync::Mutex<ProgressAggState>>) -> Self {
        Self {
            state,
            file_total_contributed: 0,
            file_done_contributed: 0,
        }
    }
}

impl HfProgress for ProgressCallback {
    fn init(&mut self, size: usize, _filename: &str) {
        let new_total = size as u64;
        let mut s = self.state.lock().expect("progress mutex poisoned");
        if new_total > self.file_total_contributed {
            let delta = new_total - self.file_total_contributed;
            s.total_bytes = s.total_bytes.saturating_add(delta);
            self.file_total_contributed = new_total;
        }
        // On retry, hf-hub re-calls `init` and then `update(resume_offset)`.
        // Reset the per-file done counter so that `update` deltas align
        // with what's actually been transferred since the (possibly
        // partial) restart.
        self.file_done_contributed = 0;
        emit(&s);
    }

    fn update(&mut self, size: usize) {
        let new_done = self.file_done_contributed.saturating_add(size as u64);
        let clamped = new_done.min(self.file_total_contributed);
        let delta = clamped.saturating_sub(self.file_done_contributed);
        self.file_done_contributed = clamped;
        if delta == 0 {
            return;
        }
        let mut s = self.state.lock().expect("progress mutex poisoned");
        s.done_bytes = s.done_bytes.saturating_add(delta);
        emit(&s);
    }

    fn finish(&mut self) {
        // Settle any sub-chunk rounding so the bar lands exactly on
        // 100 % for this file before the next one's `init` arrives.
        if self.file_done_contributed < self.file_total_contributed {
            let remainder = self.file_total_contributed - self.file_done_contributed;
            self.file_done_contributed = self.file_total_contributed;
            let mut s = self.state.lock().expect("progress mutex poisoned");
            s.done_bytes = s.done_bytes.saturating_add(remainder);
            emit(&s);
        }
    }
}

fn emit(s: &ProgressAggState) {
    if let Some(tx) = &s.sender {
        // Best-effort send. The channel is bounded (32) — if the
        // UI is briefly behind we'd rather drop a tick than block
        // the download loop.
        let _ = tx.try_send(OcrProgressEvent::DownloadProgress {
            done: s.done_bytes,
            total: if s.total_bytes > 0 {
                Some(s.total_bytes)
            } else {
                None
            },
        });
    }
}

#[async_trait]
impl OcrBackend for MistralRsBackend {
    fn name(&self) -> &str {
        &self.name
    }

    async fn transcribe_pages(
        &self,
        pages: Vec<Vec<u8>>,
        opts: &TranscribeOptions,
        progress: Option<mpsc::Sender<OcrProgressEvent>>,
        cancel: OcrCancel,
    ) -> Result<Vec<PageTranscript>, OcrError> {
        let mut out = Vec::with_capacity(pages.len());
        let prompt = build_prompt(opts);

        for (idx, png_bytes) in pages.into_iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(OcrError::Cancelled);
            }
            if let Some(p) = &progress {
                let _ = p.send(OcrProgressEvent::PageStarted { page_index: idx }).await;
            }

            let image = image::load_from_memory(&png_bytes).map_err(|e| {
                OcrError::Render(format!("page {idx} not a decodable image: {e}"))
            })?;

            let messages = mistralrs::MultimodalMessages::new().add_image_message(
                TextMessageRole::User,
                prompt.clone(),
                vec![image],
            );

            let response = self
                .model
                .send_chat_request(messages)
                .await
                .map_err(|e| OcrError::Inference(format!("page {idx}: {e}")))?;

            let text = response
                .choices
                .first()
                .and_then(|c| c.message.content.clone())
                .unwrap_or_default()
                .trim()
                .to_string();

            if let Some(p) = &progress {
                let _ = p
                    .send(OcrProgressEvent::PageDone {
                        page_index: idx,
                        chars: text.chars().count(),
                    })
                    .await;
            }

            out.push(PageTranscript {
                page_index: idx,
                text,
                detected_language: None,
            });
        }

        if let Some(p) = &progress {
            let total_chars: usize = out.iter().map(|p| p.text.chars().count()).sum();
            let _ = p
                .send(OcrProgressEvent::Done {
                    pages_done: out.len(),
                    total_chars,
                })
                .await;
        }
        Ok(out)
    }
}

fn build_prompt(opts: &TranscribeOptions) -> String {
    let mut p = String::from(TRANSCRIBE_PROMPT_BASE);
    if let Some(lang) = opts.language.as_deref() {
        if !lang.trim().is_empty() {
            p.push_str(&format!(
                "\n\nThe text is primarily in {lang}; recognise that script accordingly."
            ));
        }
    }
    if opts.markdown {
        p.push_str(
            "\n\nWhere the original handwriting clearly indicates structure (titles, bullet \
             lists, indented quotes), output it as Markdown. Otherwise output plain prose.",
        );
    }
    p
}
