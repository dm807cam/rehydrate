use rehydrate_core::{DeleteFolderOutcome, FolderEntry, RevertReport};
use tauri::State;

use crate::state::AppState;
use crate::util::{err, lib_arc};

#[tauri::command]
pub async fn list_folders(state: State<'_, AppState>) -> Result<Vec<FolderEntry>, String> {
    let lib = lib_arc(&state).await?;
    lib.list_folders().map_err(err)
}

/// Rename a folder. Updates the local row and flags it for push so
/// the next sync uploads the new metadata to the tablet.
#[tauri::command]
pub async fn rename_folder(
    folder_id: String,
    new_name: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let lib = lib_arc(&state).await?;
    lib.rename_folder(&folder_id, &new_name).map_err(err)?;
    Ok(())
}

/// Create a new folder under `parentId` (None = root). The folder is
/// flagged for push so the next sync uploads its `<uuid>.metadata`
/// file to the tablet.
#[tauri::command]
pub async fn create_folder(
    visible_name: String,
    parent_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<FolderEntry, String> {
    let lib = lib_arc(&state).await?;
    lib.create_folder(&visible_name, parent_id.as_deref())
        .map_err(err)
}

/// Roll back every local edit since the last successful sync —
/// folder renames, reparents, deletions, creations, and document
/// move/rename metadata changes. Imports are intentionally not
/// touched. Returns a precise tally so the UI can confirm what was
/// undone. See `Library::revert_unpushed_changes` for the policy.
#[tauri::command]
pub async fn revert_unpushed_changes(
    state: State<'_, AppState>,
) -> Result<RevertReport, String> {
    let lib = lib_arc(&state).await?;
    lib.revert_unpushed_changes().map_err(err)
}

/// Delete a folder from the local library and queue a tombstone
/// push so the tablet drops it on the next sync. Contents are
/// preserved: every direct child folder and document is reparented
/// to the deleted folder's parent (root if it was already at root).
/// Returns the count of moved children so the UI can word the
/// confirmation toast precisely.
#[tauri::command]
pub async fn delete_folder(
    folder_id: String,
    state: State<'_, AppState>,
) -> Result<DeleteFolderOutcome, String> {
    let lib = lib_arc(&state).await?;
    lib.delete_folder(&folder_id).map_err(err)
}

/// Move and/or reorder a folder in the sidebar. Sort order is
/// local-only (the tablet has no notion of sibling order), but
/// reparenting (changing `new_parent`) IS represented on the device
/// through each folder's `<uuid>.metadata` `parent` field — the
/// library layer rewrites the cached metadata and flags the folder
/// for push when the parent actually changes.
#[tauri::command]
pub async fn reorder_folder(
    folder_id: String,
    new_parent: Option<String>,
    new_sort_index: f64,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let lib = lib_arc(&state).await?;
    lib.reorder_folder(&folder_id, new_parent.as_deref(), new_sort_index)
        .map_err(err)
}
