//! On-device OCR for reMarkable notebooks.
//!
//! The heavy VLM runtime is feature-gated: `mistral` opts in to
//! `mistralrs` + Metal kernels via `mistral.rs`, which adds 5–10
//! minutes to a cold workspace build. Without the feature the
//! crate ships only the backend trait and the `Mock` impl —
//! enough for unit tests and dev iteration on the rest of the
//! workspace.
//!
//! Privacy invariant: outbound HTTP only fires on user-explicit
//! action. The mistral.rs path triggers HuggingFace downloads via
//! `hf-hub`; the legacy `model_store` path uses ureq against the
//! same allow-list. No network on launch.

pub mod backend;
#[cfg(feature = "mistral")]
pub mod mistral;
pub mod model_store;
pub mod page_render;
pub mod progress;

pub use backend::{Mock, OcrBackend, OcrCancel, OcrError, PageTranscript, TranscribeOptions};
#[cfg(feature = "mistral")]
pub use mistral::{MistralRsBackend, DEFAULT_MODEL_ID};
pub use model_store::{ModelDescriptor, ModelStatus, ModelStore};
pub use page_render::render_rm_to_png;
pub use progress::OcrProgressEvent;

/// Public default model identifier surfaced to the IPC layer. With
/// the `mistral` feature this is the HuggingFace repo ID for the
/// unquantized Qwen2.5-VL-3B-Instruct. Runtime ISQ-Q4 (AFQ4 on
/// Metal, Q4K on CUDA) brings resident memory down to ~2 GB but
/// the user still pays ~6 GB on the first download — no
/// pre-quantized variant of this model loads correctly on Apple
/// Silicon via mistral.rs today (the official AWQ build needs
/// CUDA kernels, and no AFQ build is published).
pub fn default_model_id() -> &'static str {
    #[cfg(feature = "mistral")]
    {
        DEFAULT_MODEL_ID
    }
    #[cfg(not(feature = "mistral"))]
    {
        "Qwen/Qwen2.5-VL-3B-Instruct"
    }
}

/// Approximate weights size in bytes for the default model — used
/// only to size progress bars in the UI before any download
/// reports an actual Content-Length.
pub fn default_model_size_hint() -> u64 {
    // Qwen2.5-VL-3B-Instruct safetensors total ≈ 6 GB across two
    // shards. ISQ runs at load time, so the user pays the full
    // download cost once.
    6_000_000_000
}
