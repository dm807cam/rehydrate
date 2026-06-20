use std::path::PathBuf;

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

use crate::logging;
use crate::util::err;

#[derive(Serialize)]
pub struct LogTail {
    pub lines: Vec<String>,
    pub log_dir: Option<PathBuf>,
}

#[tauri::command]
pub async fn get_recent_logs(max_lines: Option<usize>) -> Result<LogTail, String> {
    let n = max_lines.unwrap_or(500);
    let lines = logging::read_tail(n).map_err(err)?;
    Ok(LogTail {
        lines,
        log_dir: logging::log_dir(),
    })
}

/// Open the rolling-log directory in the OS file manager (Finder
/// on macOS). The path comes from `logging::log_dir()` so the
/// renderer can't influence which directory gets revealed — no
/// path traversal surface.
///
/// Returns the resolved path so the UI can present a fallback
/// (toast with the path string) if the open call fails — e.g.
/// when the directory doesn't exist yet because no logs have been
/// written this session.
#[tauri::command]
pub async fn reveal_log_dir(app: AppHandle) -> Result<String, String> {
    let dir = logging::log_dir().ok_or_else(|| "no log directory on this platform".to_string())?;
    let path = dir.to_string_lossy().to_string();
    app.opener()
        .open_path(&path, None::<&str>)
        .map_err(|e| format!("could not reveal {path}: {e}"))?;
    Ok(path)
}
