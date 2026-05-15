use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use rehydrate_core::Library;
use rehydrate_device::ssh::SshDevice;
use rehydrate_device::DeviceInfo;
use rehydrate_ocr::OcrCancel;
use rehydrate_sync::Cancel as SyncCancel;
use tokio::sync::{Mutex, RwLock};

/// Shared application state. Held in `tauri::State<AppState>` and accessed
/// from async command handlers; we use tokio::Mutex everywhere so locks can
/// be held across .await points.
///
/// The `library` is held as `Arc<Library>` rather than `Library` directly
/// so command handlers can clone the Arc out of the mutex briefly and
/// then run long operations against `&*library` without keeping any other
/// command blocked. Library itself is `Sync`, so `&Library` is `Send` and
/// can cross `.await` points.
pub struct AppState {
    pub library: Mutex<Option<Arc<Library>>>,
    pub library_path: Mutex<Option<PathBuf>>,
    /// Path the user explicitly picked through `pick_library_directory`'s
    /// server-side OS folder picker but has not yet opened. Consumed
    /// one-shot by `open_library` so the backend can enforce the
    /// pick → open contract — a renderer-side XSS that calls
    /// `ipc.openLibrary("/tmp/foo")` without first going through the
    /// picker hits the allowlist gate and is rejected (issue #36).
    /// Cleared on consumption, on a subsequent pick that overwrites it,
    /// or on a successful `open_library` for any allowed source.
    pub pending_picked_path: Mutex<Option<PathBuf>>,
    pub device: Mutex<Option<Arc<SshDevice>>>,
    pub device_info: RwLock<Option<DeviceInfo>>,
    pub device_reachable: RwLock<bool>,
    /// Cached result of the most recent Ollama reachability probe.
    /// `transcribe_document` consults this before making a real
    /// request; cache TTL is `OLLAMA_PING_TTL` (30 s) so we don't
    /// re-probe on every OCR action while the user is mid-batch.
    pub last_ollama_ping: RwLock<Option<OllamaPing>>,
    /// Set to `true` at app exit. Long-lived background tasks (the
    /// reachability watcher, progress forwarders) consult this each
    /// loop and bail out cleanly instead of holding the `AppHandle`
    /// across the runtime's teardown.
    pub shutdown_requested: Arc<AtomicBool>,
    /// Cancellation handle for the currently-running sync, exposed
    /// through the `cancel_sync` Tauri command so the renderer's
    /// cancel button can trip it. A single long-lived handle is
    /// reset before each sync (Cancel::reset); this lets the
    /// renderer flip cancellation without race-prone token plumbing
    /// through `cancel_sync` → `state` → in-flight handler.
    pub sync_cancel: SyncCancel,
    /// Cancellation handle for the currently-running OCR job, same
    /// shape as `sync_cancel` above. Tripped by `cancel_ocr`.
    pub ocr_cancel: OcrCancel,
}

#[derive(Debug, Clone)]
pub struct OllamaPing {
    pub at: Instant,
    pub base_url: String,
    /// Strictly: did the daemon answer the last probe? This is NOT
    /// "model present" and NOT "OCR succeeded" — overloading the
    /// field with those meanings is what caused users to see
    /// spurious "Ollama unreachable" errors after a missing-model
    /// status check or a single transient request failure. Every
    /// writer must record reachability only; model availability is
    /// a separate, recomputed-on-demand concern.
    pub reachable: bool,
}

/// How long a successful `ping_ollama` result is trusted before a
/// new probe is needed. Short enough that the user starting Ollama
/// after a failed transcribe isn't stuck waiting for the cache to
/// expire; long enough that batched OCR over many docs reuses one
/// probe.
pub const OLLAMA_PING_TTL: std::time::Duration = std::time::Duration::from_secs(30);

impl AppState {
    pub fn new() -> Self {
        Self {
            library: Mutex::new(None),
            library_path: Mutex::new(None),
            pending_picked_path: Mutex::new(None),
            device: Mutex::new(None),
            device_info: RwLock::new(None),
            device_reachable: RwLock::new(false),
            last_ollama_ping: RwLock::new(None),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            sync_cancel: SyncCancel::default(),
            ocr_cancel: OcrCancel::new(),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Default cross-platform library location: `<user docs>/reHydrate` on
/// platforms that have a documents dir, falling back to `<home>/reHydrate`.
pub fn default_library_dir() -> Option<PathBuf> {
    if let Some(dirs) = directories::UserDirs::new() {
        if let Some(docs) = dirs.document_dir() {
            return Some(docs.join("reHydrate"));
        }
    }
    directories::BaseDirs::new().map(|b| b.home_dir().join("reHydrate"))
}

pub const KEYRING_SERVICE: &str = "reHydrate";
pub const KEYRING_DEVICE_USER: &str = "remarkable-usb-password";
/// Keychain slot for Ghost admin API credentials. JSON-encoded
/// `rehydrate_publish::GhostCredentials`.
pub const KEYRING_GHOST_CREDS: &str = "ghost-admin-credentials";
/// Keychain slot for WordPress credentials. JSON-encoded
/// `rehydrate_publish::WordpressCredentials`.
pub const KEYRING_WORDPRESS_CREDS: &str = "wordpress-application-password";
