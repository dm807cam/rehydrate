use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use crate::state::AppState;

#[tauri::command]
pub fn ping() -> &'static str {
    "pong"
}

/// Open an https URL in the user's default browser. The renderer
/// uses this to surface the support / Issues link from the About
/// dialog. Restricted to https://github.com/dm807cam/rehydrate/ so a
/// compromised renderer can't open arbitrary websites — the opener
/// plugin would happily launch any URL, but that's not a
/// capability the App needs exposed.
#[tauri::command]
pub async fn open_support_url(url: String, app: AppHandle) -> Result<(), String> {
    const SUPPORT_PREFIX: &str = "https://github.com/dm807cam/rehydrate/";
    if !url.starts_with(SUPPORT_PREFIX) {
        return Err(format!(
            "refusing to open URL outside support prefix ({SUPPORT_PREFIX})"
        ));
    }
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|e| format!("could not open {url}: {e}"))
}

/// Return the app's published version string. Used by the About
/// dialog. Kept distinct from any in-app autoupdate flow because
/// v1.0 has no autoupdate — this is purely diagnostic copy.
#[tauri::command]
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Trip the shared sync cancellation flag. The engine polls the
/// flag between documents (and inside the folder-push loop), so an
/// in-flight sync_two_way / pull_execute / push_execute stops at
/// the next granular boundary and returns `SyncError::Cancelled`.
/// Idempotent: calling when nothing is running is a no-op (the next
/// sync resets the flag before it starts).
#[tauri::command]
pub fn cancel_sync(state: State<'_, AppState>) -> Result<(), String> {
    state.sync_cancel.cancel();
    Ok(())
}

/// Trip the shared OCR cancellation flag. Mirrors `cancel_sync`
/// but targets `transcribe_document` / the auto-OCR sweep. The
/// flag is reset at the start of each new OCR job.
#[tauri::command]
pub fn cancel_ocr(state: State<'_, AppState>) -> Result<(), String> {
    state.ocr_cancel.cancel();
    Ok(())
}
