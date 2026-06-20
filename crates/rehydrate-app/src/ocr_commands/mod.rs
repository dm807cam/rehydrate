//! IPC commands for the OCR + publish pipeline. Lives in its own
//! module so `commands/` doesn't grow into a wall.
//!
//! Pattern matches the rest of the IPC layer:
//! - `lib_arc(&state)` for the open library.
//! - `tauri::async_runtime::spawn_blocking` for sync work (Ollama
//!   HTTP, page rendering, publish API).
//! - Progress streamed via `app.emit("ocr:*", &event)`.
//! - Server-side dialog pickers for save targets so the renderer
//!   can't aim writes at arbitrary paths (audit fix H7 pattern).
//!
//! OCR backend: built per call from `AppConfig.ollama` (no long-lived
//! `Arc<dyn OcrBackend>` in `AppState`). `OllamaBackend::new` is
//! cheap — one `ureq::AgentBuilder` — and per-call construction
//! means a settings change takes effect immediately without a
//! reload step.
//!
//! Submodule layout:
//!   - `ollama`     — config, ping, status, curated models, reachability
//!                    cache, default-model lookup
//!   - `transcribe` — transcribe_document, get_transcript,
//!                    list_documents_needing_ocr, frontmatter helpers
//!   - `export_md`  — export_transcript (to .txt / .md)
//!   - `publish`    — Ghost / WordPress credentials + publish_transcript
//!                    + open_publish_url + ping_publish_target

mod export_md;
mod ollama;
mod publish;
mod transcribe;

pub use export_md::*;
pub use ollama::*;
pub use publish::*;
pub use transcribe::*;

/// Path under which the transcript markdown is stored as a derived
/// artefact next to each document. Shared between every submodule
/// that reads or writes the transcript (`transcribe`, `export_md`,
/// `publish`, plus the OCR-candidate scan in `transcribe`).
pub(super) const TRANSCRIPT_PATH: &str = "ocr/transcript.md";
