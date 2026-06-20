use std::path::PathBuf;
use std::sync::Arc;

use rehydrate_core::{GarbageCollectReport, Library, VerifyReport};
use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

use crate::config;
use crate::state::{default_library_dir, AppState};
use crate::util::{err, lib_arc};

#[derive(Serialize)]
pub struct LibrarySummary {
    pub path: PathBuf,
    pub document_count: usize,
    pub version_count: i64,
    pub blob_count: usize,
    pub size_bytes: u64,
}

#[derive(Serialize)]
pub struct PickedLibraryDirectory {
    pub path: PathBuf,
    pub kind: PickedLibraryKind,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PickedLibraryKind {
    Empty,
    Existing,
}

#[derive(Serialize)]
pub struct RecentLibraryEntry {
    pub path: PathBuf,
    pub label: String,
    pub last_opened: String,
    /// True if `path` still exists and looks like a library on disk.
    /// The UI uses this to grey out stale entries.
    pub available: bool,
    /// True if this is the currently-open library.
    pub current: bool,
}

#[tauri::command]
pub fn default_library_path() -> Option<PathBuf> {
    default_library_dir()
}

#[tauri::command]
pub async fn open_library(path: PathBuf, state: State<'_, AppState>) -> Result<(), String> {
    // Allowlist gate (issue #36). `switch_library` already requires the
    // target to be in `recent_libraries`; `open_library` was the only
    // remaining entry point that accepted an arbitrary renderer-supplied
    // path. A renderer XSS that called `ipc.openLibrary("/tmp/foo")`
    // would stamp a fresh library at any path, persist it to
    // config.json, and have `auto_open_library` silently re-open the
    // attacker-chosen path on next launch — shadowing the user's real
    // library. The legitimate user-driven flows are (1) accepting the
    // default-dir suggestion from the welcome screen and (2) running
    // the server-side OS folder picker; gate on those two plus the
    // existing recents allowlist. The pending-pick slot is one-shot:
    // a successful open consumes it so a stale token can't be replayed.
    let cfg = config::load();
    let mut pending = state.pending_picked_path.lock().await;
    let allowed = is_allowed_open_path(&path, &cfg.recent_libraries, pending.as_deref());
    if !allowed {
        return Err(format!(
            "{} is not an approved library path; use 'Open another library…' to pick it first",
            path.display()
        ));
    }
    let was_pending_match = pending.as_deref() == Some(path.as_path());
    open_library_at(&path, &state).await?;
    if was_pending_match {
        *pending = None;
    }
    Ok(())
}

/// Return true iff `path` is allowed as an `open_library` target —
/// extracted so the gate logic can be exercised by a unit test without
/// constructing a Tauri runtime. The three legitimate sources:
/// 1. The path the user just confirmed in the server-side OS picker
///    (`pending`, consumed one-shot by the caller on success).
/// 2. Any path the user has previously opened (`recents`), since each
///    of those once went through (1) before being recorded.
/// 3. The cross-platform default library dir, so the welcome-screen
///    "use defaults" button works on first launch when recents is
///    empty and the user has not yet engaged the picker.
fn is_allowed_open_path(
    path: &std::path::Path,
    recents: &[crate::config::RecentLibrary],
    pending: Option<&std::path::Path>,
) -> bool {
    if pending == Some(path) {
        return true;
    }
    if recents.iter().any(|r| r.path == path) {
        return true;
    }
    if let Some(default) = crate::state::default_library_dir() {
        if default == path {
            return true;
        }
    }
    false
}

/// Common path-validated open used by `open_library`, `switch_library`,
/// and `switch_library_via_dialog`. Drops the previously-held library
/// (and its OS lock) before opening the new one, so the same process
/// can move between per-device libraries without restarting.
async fn open_library_at(
    path: &std::path::Path,
    state: &State<'_, AppState>,
) -> Result<(), String> {
    // Drop the existing Library first so its `.lock` is released. If
    // the user is switching to the SAME library, this avoids
    // `AlreadyOpen` when we re-open it below.
    {
        let mut slot = state.library.lock().await;
        *slot = None;
    }
    let lib = Library::open(path).map_err(err)?;
    *state.library.lock().await = Some(Arc::new(lib));
    *state.library_path.lock().await = Some(path.to_path_buf());

    // Persist for next launch + push to recents. Best-effort.
    let mut cfg = config::load();
    cfg.record_open(path.to_path_buf());
    if let Err(e) = config::save(&cfg) {
        tracing::warn!("failed to persist app config: {e}");
    }
    Ok(())
}

/// On launch the UI calls this to restore the previously-opened library
/// without forcing the user through the welcome screen each time. Returns
/// the path that was opened, or `None` if there was no valid prior library.
#[tauri::command]
pub async fn auto_open_library(state: State<'_, AppState>) -> Result<Option<PathBuf>, String> {
    let mut cfg = config::load();
    let Some(path) = cfg.library_path.clone() else {
        return Ok(None);
    };
    if !path.exists() {
        // Stale config — drop the entry silently so the user sees the
        // welcome screen again rather than a confusing error.
        cfg.library_path = None;
        cfg.recent_libraries.retain(|r| r.path != path);
        let _ = config::save(&cfg);
        return Ok(None);
    }
    open_library_at(&path, &state).await?;
    Ok(Some(path))
}

/// Switch to a previously-opened library by path. The path must
/// already be in the recents list (so the user has consciously opened
/// it before via the picker). Returns the path opened, or an error if
/// the library is no longer there or its stamp is invalid.
#[tauri::command]
pub async fn switch_library(path: PathBuf, state: State<'_, AppState>) -> Result<PathBuf, String> {
    let cfg = config::load();
    if !cfg.recent_libraries.iter().any(|r| r.path == path) {
        return Err(format!(
            "{} is not in the recent libraries list; use 'Open another library…' to add it",
            path.display()
        ));
    }
    open_library_at(&path, &state).await?;
    Ok(path)
}

/// Open the OS folder picker so the user can pick a library
/// directory, then probe the chosen path. Returns:
/// - `None` if the user cancelled the dialog;
/// - `Some({ path, kind: "existing" })` if the path is already a
///   stamped library — the renderer should call `open_library` to
///   open it without further confirmation;
/// - `Some({ path, kind: "empty" })` if the path is an empty (or
///   dotfile-only) directory — the renderer should ask the user
///   "Create a new library here?" before calling `open_library`.
///
/// Foreign-but-non-empty directories surface as `Err`. We never
/// open the library here so a confirm prompt can sit between the
/// pick and the side-effecting open.
#[tauri::command]
pub async fn pick_library_directory(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<PickedLibraryDirectory>, String> {
    let app_for_pick = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_pick
            .dialog()
            .file()
            .set_title("Open a reHydrate library folder")
            .blocking_pick_folder()
    })
    .await
    .map_err(err)?;

    let Some(file_path) = picked else {
        return Ok(None);
    };
    let path = file_path
        .into_path()
        .map_err(|e| format!("could not resolve picked folder: {e}"))?;

    // Probe synchronously — no lock acquired, no library.json
    // written — so the renderer's confirm prompt sits between this
    // and the actual `open_library`.
    let kind = match Library::probe_path(&path) {
        Ok(rehydrate_core::LibraryPathKind::Empty) => PickedLibraryKind::Empty,
        Ok(rehydrate_core::LibraryPathKind::Existing) => PickedLibraryKind::Existing,
        Err(e) => return Err(err(e)),
    };
    // Mark this path as user-approved for one subsequent `open_library`
    // (issue #36). Overwrites any previous pending pick so the latest
    // user intent wins. `open_library` consumes the slot on success.
    *state.pending_picked_path.lock().await = Some(path.clone());
    Ok(Some(PickedLibraryDirectory { path, kind }))
}

/// Folder picker for "where do you want to save exported PDFs?". Returns
/// `Ok(None)` if the user cancelled. Unlike `pick_library_directory`,
/// this is fire-and-forget — no probing, no `pending_picked_path` slot.
/// The export command takes the returned path verbatim as its
/// `target_dir` argument.
#[tauri::command]
pub async fn pick_export_directory(app: AppHandle) -> Result<Option<PathBuf>, String> {
    let app_for_pick = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app_for_pick
            .dialog()
            .file()
            .set_title("Export PDFs to…")
            .blocking_pick_folder()
    })
    .await
    .map_err(err)?;

    let Some(file_path) = picked else {
        return Ok(None);
    };
    let path = file_path
        .into_path()
        .map_err(|e| format!("could not resolve picked folder: {e}"))?;
    Ok(Some(path))
}

#[tauri::command]
pub async fn list_recent_libraries(
    state: State<'_, AppState>,
) -> Result<Vec<RecentLibraryEntry>, String> {
    let cfg = config::load();
    let active = state.library_path.lock().await.clone();
    Ok(cfg
        .recent_libraries
        .into_iter()
        .map(|r| {
            let available = r.path.is_dir() && r.path.join("library.json").is_file();
            let current = active.as_ref() == Some(&r.path);
            RecentLibraryEntry {
                path: r.path,
                label: r.label,
                last_opened: r.last_opened,
                available,
                current,
            }
        })
        .collect())
}

#[tauri::command]
pub async fn garbage_collect(state: State<'_, AppState>) -> Result<GarbageCollectReport, String> {
    let lib = lib_arc(&state).await?;
    lib.garbage_collect().map_err(err)
}

#[tauri::command]
pub async fn verify_library(state: State<'_, AppState>) -> Result<VerifyReport, String> {
    let lib = lib_arc(&state).await?;
    lib.verify().map_err(err)
}

#[tauri::command]
pub async fn library_summary(state: State<'_, AppState>) -> Result<LibrarySummary, String> {
    let lib = lib_arc(&state).await?;
    let path = state
        .library_path
        .lock()
        .await
        .clone()
        .ok_or_else(|| "no library is open".to_string())?;
    let docs = lib.list_documents().map_err(err)?;
    // `blob_stats` walks the blob store from disk — for a large library
    // that's tens of thousands of fanout entries to stat. Done inline
    // it stalls the Tauri command thread (and on a single-threaded
    // executor, every other IPC call). Move to `spawn_blocking` so
    // the runtime stays responsive while we count.
    let blob_path = path.clone();
    let (blob_count, size_bytes) =
        tauri::async_runtime::spawn_blocking(move || blob_stats(&blob_path))
            .await
            .map_err(err)?;
    let version_count = lib.version_count().map_err(err)?;
    Ok(LibrarySummary {
        path,
        document_count: docs.len(),
        version_count,
        blob_count,
        size_bytes,
    })
}

fn blob_stats(library_root: &std::path::Path) -> (usize, u64) {
    let blobs = library_root.join("blobs");
    let mut count = 0usize;
    let mut size = 0u64;
    walk(&blobs, &mut |path| {
        if let Ok(meta) = std::fs::metadata(path) {
            count += 1;
            size += meta.len();
        }
    });
    (count, size)
}

/// Hard cap on `walk` recursion depth. The blob store is a two-level
/// fanout (`blobs/<aa>/<bb>/<hash>`) so a sane tree never exceeds 3 —
/// anything past the cap is a malformed (or hostile) directory, and
/// without the bound a symlink loop the os layer doesn't filter could
/// blow the stack inside an `async` command.
const BLOB_WALK_MAX_DEPTH: usize = 8;

fn walk(p: &std::path::Path, f: &mut impl FnMut(&std::path::Path)) {
    walk_inner(p, f, 0);
}

fn walk_inner(p: &std::path::Path, f: &mut impl FnMut(&std::path::Path), depth: usize) {
    if depth > BLOB_WALK_MAX_DEPTH {
        return;
    }
    let rd = match std::fs::read_dir(p) {
        Ok(r) => r,
        Err(e) => {
            // Surface — but don't crash — failures mid-walk so an
            // orphan-blob undercount is visible in the operations log
            // rather than silently shrinking the report's blob_count.
            tracing::warn!(path = %p.display(), "read_dir failed during blob walk: {e}");
            return;
        }
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        // Skip symlinks: the blob store should never contain them, and
        // following a planted symlink could let the walker enumerate
        // files outside the library root.
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk_inner(&path, f, depth + 1);
        } else {
            f(&path);
        }
    }
}

#[cfg(test)]
mod open_library_allowlist_tests {
    use super::is_allowed_open_path;
    use crate::config::RecentLibrary;
    use std::path::{Path, PathBuf};

    fn rec(path: &str) -> RecentLibrary {
        RecentLibrary {
            path: PathBuf::from(path),
            label: "test".into(),
            last_opened: String::new(),
        }
    }

    /// Issue #36 regression: the bare-minimum attack shape. With no
    /// recents and no pending pick, an arbitrary renderer path must
    /// be refused. The default-dir allowance is intentionally narrow
    /// (`default_library_dir()` is the cross-platform default and is
    /// host-machine-dependent), so this test picks `/tmp/attacker`
    /// which is never the default.
    #[test]
    fn arbitrary_path_with_no_recents_no_pending_is_rejected() {
        let recents: Vec<RecentLibrary> = vec![];
        assert!(!is_allowed_open_path(
            Path::new("/tmp/attacker"),
            &recents,
            None,
        ));
    }

    /// The path the user just confirmed in the server-side OS picker
    /// is allowed. This is the post-`pick_library_directory` happy
    /// path that the renderer hits via `openAnotherLibrary` in
    /// `App.tsx`.
    #[test]
    fn pending_pick_path_is_allowed() {
        let recents: Vec<RecentLibrary> = vec![];
        let pending = PathBuf::from("/Users/test/MyLib");
        assert!(is_allowed_open_path(
            &pending,
            &recents,
            Some(pending.as_path()),
        ));
    }

    /// A different path while a pick is pending must still be refused
    /// — the pending slot only authorises the exact path it points at,
    /// not "any path while a pick happens to be pending."
    #[test]
    fn pending_pick_does_not_authorise_a_different_path() {
        let recents: Vec<RecentLibrary> = vec![];
        let pending = PathBuf::from("/Users/test/MyLib");
        assert!(!is_allowed_open_path(
            Path::new("/tmp/attacker"),
            &recents,
            Some(pending.as_path()),
        ));
    }

    /// Recents are the persisted form of "user previously approved
    /// this." A path on the recents list is allowed even with no
    /// active pending pick — covers the welcome screen re-opening a
    /// known library and `switch_library`'s equivalent gate.
    #[test]
    fn recents_path_is_allowed_without_pending() {
        let recents = vec![rec("/Users/test/PriorLib")];
        assert!(is_allowed_open_path(
            Path::new("/Users/test/PriorLib"),
            &recents,
            None,
        ));
    }

    /// A path that *looks* similar to a recents entry but isn't byte-
    /// equal must NOT match. Sibling-directory escape is the obvious
    /// attack shape: if recents contains `/Users/test/Lib`, an open
    /// against `/Users/test/Lib2` or `/Users/test/Lib/../Other` must
    /// fall through to the deny branch. (Renderer-sent paths aren't
    /// canonicalised here; the picker round-trip preserves whatever
    /// the OS returned, so byte-equality is the right comparison.)
    #[test]
    fn near_miss_recents_path_is_rejected() {
        let recents = vec![rec("/Users/test/Lib")];
        assert!(!is_allowed_open_path(
            Path::new("/Users/test/Lib2"),
            &recents,
            None,
        ));
        assert!(!is_allowed_open_path(
            Path::new("/Users/test/Lib/../Other"),
            &recents,
            None,
        ));
    }

    /// The default library dir is always allowed (welcome-screen
    /// "use defaults" button on first launch). Skip if the test host
    /// has no resolvable default — CI on the self-hosted macOS runner
    /// always does, but unit tests should still be portable.
    #[test]
    fn default_library_dir_is_allowed() {
        let Some(default) = crate::state::default_library_dir() else {
            return;
        };
        let recents: Vec<RecentLibrary> = vec![];
        assert!(is_allowed_open_path(&default, &recents, None));
    }
}
