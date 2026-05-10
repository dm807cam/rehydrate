//! Progress event types streamed from OCR + model-download flows
//! through `tokio::mpsc` channels to the IPC layer's
//! `app.emit("ocr:*", ...)` calls.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OcrProgressEvent {
    /// Backend has started rendering / inference for a page.
    PageStarted { page_index: usize },
    /// One page produced N characters of transcript.
    PageDone { page_index: usize, chars: usize },
    /// One page failed; the run continues with the rest.
    PageFailed {
        page_index: usize,
        message: String,
    },
    /// Whole run finished (success or cancelled). Used to close out
    /// the renderer's "running" UI state cleanly.
    Done {
        pages_done: usize,
        total_chars: usize,
    },
    /// Model download is in flight. `total` is the sum of file sizes
    /// for the repo's snapshot; `done` is bytes copied to disk so far.
    /// `total` may still be `None` if the metadata probe couldn't
    /// determine sizes — the UI then falls back to indeterminate.
    DownloadProgress { done: u64, total: Option<u64> },
    /// Bytes are on disk; the runtime is now mapping the safetensors
    /// shards and applying ISQ. Surfaces a distinct UI phase from the
    /// download itself, since this step alone takes minutes on a
    /// consumer machine and otherwise looks like a hang.
    ModelLoading,
    /// Model is fully loaded into memory and ready to serve.
    DownloadDone,
}
