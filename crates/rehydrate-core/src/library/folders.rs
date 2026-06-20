//! Library API for folder lifecycle: list / upsert / create / reorder /
//! rename / delete, plus the push-queue helpers
//! (`list_pending_folder_pushes` / `mark_folder_pushed`).

#![allow(unused_imports)]

use std::fs;
use std::path::Path;
use std::sync::Mutex;

#[allow(unused_imports)]
use fs4::FileExt;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::blob::BlobStore;
use crate::db::Db;
use crate::error::{CoreError, Result};
use crate::hash::Sha256Hex;
use crate::manifest::{Manifest, ManifestFile};
use crate::paths::LibraryPaths;

use super::*;

impl Library {

    /// Return all folders known to the library. Sort order is the
    /// local-only `sort_index`, with `visible_name` as the tie-breaker
    /// so newly-pulled folders that all share `sort_index = 0` still
    /// land alphabetically.
    pub fn list_folders(&self) -> Result<Vec<FolderEntry>> {
        let conn = self.db.lock();
        // Tombstoned rows (`deleted_locally = 1`) are still in the
        // table because the next sync has to ship their
        // `<uuid>.metadata` with `deleted: true` so the tablet GCs
        // them; the sidebar shouldn't show them in the meantime.
        let mut stmt = conn.prepare(
            "SELECT folder_id, parent, visible_name, sort_index FROM folders \
             WHERE deleted_locally = 0 \
             ORDER BY sort_index ASC, visible_name ASC",
        )?;
        let rows: Vec<FolderEntry> = stmt
            .query_map([], |r| {
                let parent: Option<String> = r.get::<_, Option<String>>(1)?;
                Ok(FolderEntry {
                    folder_id: r.get(0)?,
                    parent: parent.filter(|s| !s.is_empty()),
                    visible_name: r.get(2)?,
                    sort_index: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }


    /// Mirror a folder from the device into the library's folder index.
    /// Resets `pending_push` to 0 because we just got authoritative
    /// state from the tablet. Preserves the local `sort_index` if the
    /// row already exists — folder order is local-only, so a re-pull
    /// of an unchanged folder must not reset the user's reorderings.
    pub fn upsert_folder(
        &self,
        folder_id: &str,
        parent: Option<&str>,
        visible_name: &str,
        metadata_json: &str,
    ) -> Result<()> {
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;
        // Pick a fresh sort_index for newly-seen folders so they slot
        // at the end of the sibling list rather than colliding at 0
        // and getting alphabetised on top of older rows.
        let next_sort: f64 = tx.query_row(
            "SELECT COALESCE(MAX(sort_index), 0.0) + 1.0 FROM folders WHERE COALESCE(parent,'') = COALESCE(?1,'')",
            params![parent],
            |r| r.get(0),
        )?;
        // If a row exists with `pending_push = 1`, the user renamed
        // the folder locally and the next push hasn't run yet. A
        // pull observed during that window would otherwise clobber
        // the local rename with the device's stale name. Audit fix
        // (Phase 5): when pending_push is set, refresh only the
        // structural columns (`parent`, `sort_index` we control) and
        // leave `visible_name` / `metadata_json` / `pending_push`
        // alone so the next push picks up the local rename
        // unchanged.
        // `last_synced_metadata_json` is set on every pull — the
        // device's current state IS the last-synced state. Even
        // when there's a pending local edit (we keep its
        // `metadata_json` intact via the CASE below), the snapshot
        // updates to whatever the device just sent us; otherwise a
        // user who edits, pulls, then reverts would roll back to a
        // stale pre-pull snapshot.
        tx.execute(
            "INSERT INTO folders(folder_id, parent, visible_name, metadata_json, pending_push, sort_index, last_synced_metadata_json) \
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?4) \
             ON CONFLICT(folder_id) DO UPDATE SET \
                 parent = excluded.parent, \
                 visible_name = CASE WHEN pending_push = 1 THEN visible_name ELSE excluded.visible_name END, \
                 metadata_json = CASE WHEN pending_push = 1 THEN metadata_json ELSE excluded.metadata_json END, \
                 pending_push = CASE WHEN pending_push = 1 THEN 1 ELSE 0 END, \
                 last_synced_metadata_json = excluded.metadata_json",
            params![folder_id, parent, visible_name, metadata_json, next_sort],
        )?;
        tx.commit()?;
        Ok(())
    }


    /// Create a brand-new local folder and queue it for push. The
    /// folder's `<uuid>.metadata` is uploaded on the next sync, where
    /// xochitl picks it up as a `CollectionType` entry. Returns the
    /// row that the UI can splice into its folder list without a
    /// full reload.
    pub fn create_folder(&self, visible_name: &str, parent: Option<&str>) -> Result<FolderEntry> {
        let trimmed = visible_name.trim().to_string();
        if trimmed.is_empty() {
            return Err(CoreError::InvalidArgument(
                "folder name must not be empty".into(),
            ));
        }
        let folder_id = uuid::Uuid::new_v4().to_string();
        let last_modified_ms =
            i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
                .unwrap_or(i64::MAX);
        // Mirror the schema xochitl writes for folders. `parent` is "" at
        // root, never a JSON null, because the device side reads it as a
        // string. `synced: false` and the `*modified` flags signal the
        // tablet to refresh its index after the file lands.
        let metadata = serde_json::json!({
            "visibleName": trimmed,
            "type": "CollectionType",
            "parent": parent.unwrap_or(""),
            "lastModified": last_modified_ms.to_string(),
            "lastOpened": "",
            "version": 1,
            "pinned": false,
            "synced": false,
            "modified": true,
            "deleted": false,
            "metadatamodified": true,
        });
        let metadata_json = serde_json::to_string(&metadata)?;

        let _write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;
        // Reject inserts under a non-existent parent — the UI shouldn't
        // ever ask for this, but failing fast keeps orphans out of the
        // tree.
        if let Some(p) = parent {
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM folders WHERE folder_id = ?1",
                params![p],
                |r| r.get(0),
            )?;
            if exists == 0 {
                return Err(CoreError::NotFound(format!("folder {p}")));
            }
        }
        // Slot at the end of the sibling list.
        let next_sort: f64 = tx.query_row(
            "SELECT COALESCE(MAX(sort_index), 0.0) + 1.0 FROM folders WHERE COALESCE(parent,'') = COALESCE(?1,'')",
            params![parent],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO folders(folder_id, parent, visible_name, metadata_json, pending_push, sort_index) \
             VALUES (?1, ?2, ?3, ?4, 1, ?5)",
            params![folder_id, parent, trimmed, metadata_json, next_sort],
        )?;
        tx.commit()?;

        Ok(FolderEntry {
            folder_id,
            parent: parent.map(str::to_string),
            visible_name: trimmed,
            sort_index: next_sort,
        })
    }


    /// Reparent and/or reorder a folder. `new_parent = None` means
    /// "move to root". `new_sort_index` is the float key used for the
    /// local-only sidebar ordering — callers compute it as the midpoint
    /// of two adjacent siblings to avoid renumbering on every drag.
    ///
    /// `sort_index` is local-only (the device has no notion of sibling
    /// order), but `parent` IS represented on the tablet through the
    /// `parent` field of each folder's `<uuid>.metadata` file. When the
    /// parent actually changes we therefore also rewrite the cached
    /// `metadata_json` and flag the folder for push, mirroring what
    /// `rename_folder` does. Without that, dragging "BH" under "Journal"
    /// in the sidebar would update only the local `parent` column —
    /// the on-device metadata would still say `parent: ""` and the
    /// next sync would surface BH at the tablet's root.
    ///
    /// Rejects moving a folder into itself or any of its descendants —
    /// such a move would create a cycle that `list_folders` cannot
    /// untangle and the sidebar tree-builder would silently drop.
    pub fn reorder_folder(
        &self,
        folder_id: &str,
        new_parent: Option<&str>,
        new_sort_index: f64,
    ) -> Result<()> {
        if !new_sort_index.is_finite() {
            return Err(CoreError::InvalidArgument(
                "sort_index must be finite".into(),
            ));
        }
        if new_parent == Some(folder_id) {
            return Err(CoreError::InvalidArgument(
                "cannot move a folder into itself".into(),
            ));
        }

        let _write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        // Confirm the row exists.
        let exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM folders WHERE folder_id = ?1",
            params![folder_id],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Err(CoreError::NotFound(format!("folder {folder_id}")));
        }

        // Cycle check: walk up from `new_parent` toward the root; if
        // we ever hit `folder_id` the move would create a loop. Capped
        // at the current folder count to avoid spinning forever on a
        // pre-existing cycle (defence in depth).
        if let Some(mut cur) = new_parent.map(str::to_string) {
            let folder_count: i64 =
                tx.query_row("SELECT COUNT(*) FROM folders", [], |r| r.get(0))?;
            for _ in 0..folder_count.max(1) {
                if cur == folder_id {
                    return Err(CoreError::InvalidArgument(
                        "cannot move a folder into one of its descendants".into(),
                    ));
                }
                let parent: Option<String> = tx
                    .query_row(
                        "SELECT parent FROM folders WHERE folder_id = ?1",
                        params![&cur],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                match parent {
                    Some(p) if !p.is_empty() => cur = p,
                    _ => break,
                }
            }
        }

        // Read the current parent + metadata_json so we know whether
        // this call is a pure reorder (no push needed) or a reparent
        // (metadata + pending_push must be refreshed).
        let (current_parent, metadata_json): (Option<String>, String) = tx.query_row(
            "SELECT parent, metadata_json FROM folders WHERE folder_id = ?1",
            params![folder_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        let reparented = current_parent.as_deref() != new_parent;

        if reparented {
            // Mirror create_folder's metadata shape: `parent` is "" at
            // root, never a JSON null, because xochitl reads it as a
            // string. Preserve the rest of the metadata object so we
            // don't clobber visibleName, pinned, etc. Defensive: if
            // the cached JSON isn't an object we rebuild a minimal one.
            let mut value: serde_json::Value =
                serde_json::from_str(&metadata_json).unwrap_or_else(|_| serde_json::json!({}));
            if !value.is_object() {
                value = serde_json::json!({});
            }
            let map = value.as_object_mut().expect("ensured above");
            map.insert(
                "parent".into(),
                serde_json::Value::String(new_parent.unwrap_or("").to_string()),
            );
            let now_ms =
                i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
                    .unwrap_or(i64::MAX);
            map.insert(
                "lastModified".into(),
                serde_json::Value::String(now_ms.to_string()),
            );
            map.insert("modified".into(), serde_json::Value::Bool(true));
            map.insert("metadatamodified".into(), serde_json::Value::Bool(true));
            map.insert("synced".into(), serde_json::Value::Bool(false));
            map.entry("type".to_string())
                .or_insert(serde_json::Value::String("CollectionType".into()));

            let updated_json = serde_json::to_string(&value)?;
            tx.execute(
                "UPDATE folders \
                 SET parent = ?1, sort_index = ?2, metadata_json = ?3, pending_push = 1 \
                 WHERE folder_id = ?4",
                params![new_parent, new_sort_index, updated_json, folder_id],
            )?;
        } else {
            // Pure reorder — sibling shuffle. No metadata change, no push.
            tx.execute(
                "UPDATE folders SET sort_index = ?1 WHERE folder_id = ?2",
                params![new_sort_index, folder_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }


    /// Rename a folder. Updates the local row + the device-facing
    /// `metadata_json` (visibleName, lastModified, modified flags) and
    /// flags the folder for push so the next sync uploads the new
    /// metadata file to the tablet.
    pub fn rename_folder(&self, folder_id: &str, new_name: &str) -> Result<()> {
        let trimmed = new_name.trim().to_string();
        if trimmed.is_empty() {
            return Err(CoreError::InvalidArgument(
                "folder name must not be empty".into(),
            ));
        }
        let _write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        let row: Option<String> = tx
            .query_row(
                "SELECT metadata_json FROM folders WHERE folder_id = ?1",
                params![folder_id],
                |r| r.get(0),
            )
            .optional()?;
        let metadata_json =
            row.ok_or_else(|| CoreError::NotFound(format!("folder {folder_id}")))?;

        // Mutate the metadata JSON in place so we keep every device
        // field (parent, lastOpened, etc.) intact. If it isn't an
        // object we still produce a minimal one — folders should
        // always have object metadata, but defensive code is cheap.
        let mut value: serde_json::Value =
            serde_json::from_str(&metadata_json).unwrap_or_else(|_| serde_json::json!({}));
        if !value.is_object() {
            value = serde_json::json!({});
        }
        let map = value.as_object_mut().expect("ensured above");
        map.insert(
            "visibleName".into(),
            serde_json::Value::String(trimmed.clone()),
        );
        // xochitl uses these to decide a re-index is needed.
        let now_ms = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
            .unwrap_or(i64::MAX);
        map.insert(
            "lastModified".into(),
            serde_json::Value::String(now_ms.to_string()),
        );
        map.insert("modified".into(), serde_json::Value::Bool(true));
        map.insert("metadatamodified".into(), serde_json::Value::Bool(true));
        map.insert("synced".into(), serde_json::Value::Bool(false));
        // Folders identify with this type on the device; preserve if
        // already set, otherwise default.
        map.entry("type".to_string())
            .or_insert(serde_json::Value::String("CollectionType".into()));

        let updated_json = serde_json::to_string(&value)?;

        tx.execute(
            "UPDATE folders SET visible_name = ?1, metadata_json = ?2, pending_push = 1 \
             WHERE folder_id = ?3",
            params![trimmed, updated_json, folder_id],
        )?;
        tx.commit()?;
        Ok(())
    }


    /// Delete a folder from the local library and queue a tombstone
    /// push so the tablet drops it on the next sync. Contents are
    /// preserved: every direct child (folders and documents) is
    /// reparented to the deleted folder's parent (or to the root
    /// if the folder was already at root). This matches the user
    /// expectation that "remove folder" leaves the notebooks intact
    /// — analogous to Finder's "Remove from folder" rather than
    /// "Move to Trash".
    ///
    /// Returns the number of (folders, documents) that were lifted
    /// out, so the UI can confirm the action with a precise
    /// "BH and test moved to <parent>" toast.
    ///
    /// Sequencing notes:
    /// * Direct child folders are reparented via `reorder_folder` so
    ///   each one's cached `metadata_json` gets its `parent` field
    ///   rewritten and its `pending_push` flagged — without that
    ///   the next sync would surface the children under the
    ///   deleted parent (the bug fixed in commit 49e935b).
    /// * Direct child documents go through `move_document`, which
    ///   records a fresh version with the updated `parent` so the
    ///   push engine ships the rewired metadata.
    /// * Only THEN is the folder itself tombstoned: cached metadata
    ///   gets `deleted: true` + the standard `lastModified` /
    ///   `modified` flags, `deleted_locally = 1`, and
    ///   `pending_push = 1`. The push engine ships that file and
    ///   `mark_folder_pushed` drops the row.
    /// * Each step calls a public API that takes its own write
    ///   lock; we don't hold one big tx because `move_document`
    ///   touches the blob store and creating a nested write lock
    ///   would deadlock. Failure partway is recoverable: every step
    ///   is idempotent, so re-invoking `delete_folder` on the same
    ///   id finishes the job.
    ///
    /// Archived documents are intentionally left alone — they're
    /// invisible to the user and not part of the sidebar tree, so
    /// their stored `parent` pointing at a tombstoned folder is
    /// harmless. If the user ever restores one, the restore path
    /// already handles dangling-parent fallback to root.
    pub fn delete_folder(&self, folder_id: &str) -> Result<DeleteFolderOutcome> {
        // 1. Resolve the folder's current parent (the "new home" for
        //    every direct child) and confirm the row exists + isn't
        //    already tombstoned. Done in a short read-only scope so
        //    the per-child API calls below don't fight for the
        //    write lock.
        let (current_parent, already_deleted): (Option<String>, i64) = {
            let conn = self.db.lock();
            conn.query_row(
                "SELECT parent, deleted_locally FROM folders WHERE folder_id = ?1",
                params![folder_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("folder {folder_id}")))?
        };
        if already_deleted == 1 {
            // Idempotent on retry — caller can re-trigger after a
            // partial failure and we won't double-tombstone.
            return Ok(DeleteFolderOutcome {
                folders_moved: 0,
                documents_moved: 0,
            });
        }
        let new_parent = current_parent
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        // 2. Collect direct child folders. Snapshot their ids +
        //    sort_indexes outside the write tx so we can iterate
        //    via the public reparent API.
        let child_folder_ids: Vec<(String, f64)> = {
            let conn = self.db.lock();
            let mut stmt = conn.prepare(
                "SELECT folder_id, sort_index FROM folders \
                 WHERE COALESCE(parent,'') = ?1 \
                   AND folder_id <> ?2 \
                   AND deleted_locally = 0",
            )?;
            let rows: Vec<(String, f64)> = stmt
                .query_map(params![folder_id, folder_id], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            rows
        };
        let folders_moved = child_folder_ids.len();
        for (child_id, sort) in &child_folder_ids {
            // reorder_folder treats a parent-change as a reparent and
            // queues a push with the rewritten metadata. Reuses sort
            // so the visible position doesn't snap to the end of the
            // list.
            self.reorder_folder(child_id, new_parent.as_deref(), *sort)?;
        }

        // 3. Documents currently in this folder. The parent lives in
        //    the manifest blob rather than a SQL column, so we have
        //    to read each one — but `list_documents` already streams
        //    them and the count is small in practice.
        let child_documents: Vec<String> = self
            .list_documents()?
            .into_iter()
            .filter(|d| d.parent.as_deref() == Some(folder_id))
            .map(|d| d.document_id)
            .collect();
        let documents_moved = child_documents.len();
        for doc_id in &child_documents {
            self.move_document(doc_id, new_parent.as_deref())?;
        }

        // 4. Tombstone the folder itself. Mirrors `rename_folder`'s
        //    metadata mutation, but flips `deleted: true` instead of
        //    rewriting `visibleName`. Holding the write_lock across
        //    the read+write keeps a concurrent rename from racing
        //    in between.
        let _write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        let metadata_json: String = tx.query_row(
            "SELECT metadata_json FROM folders WHERE folder_id = ?1",
            params![folder_id],
            |r| r.get(0),
        )?;
        let mut value: serde_json::Value =
            serde_json::from_str(&metadata_json).unwrap_or_else(|_| serde_json::json!({}));
        if !value.is_object() {
            value = serde_json::json!({});
        }
        let map = value.as_object_mut().expect("ensured above");
        map.insert("deleted".into(), serde_json::Value::Bool(true));
        let now_ms = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
            .unwrap_or(i64::MAX);
        map.insert(
            "lastModified".into(),
            serde_json::Value::String(now_ms.to_string()),
        );
        map.insert("modified".into(), serde_json::Value::Bool(true));
        map.insert("metadatamodified".into(), serde_json::Value::Bool(true));
        map.insert("synced".into(), serde_json::Value::Bool(false));
        // Defensive: keep the CollectionType marker so the device
        // still recognises the tombstone as targeting a folder.
        map.entry("type".to_string())
            .or_insert(serde_json::Value::String("CollectionType".into()));
        let updated_json = serde_json::to_string(&value)?;

        tx.execute(
            "UPDATE folders \
             SET metadata_json = ?1, pending_push = 1, deleted_locally = 1 \
             WHERE folder_id = ?2",
            params![updated_json, folder_id],
        )?;
        tx.commit()?;

        Ok(DeleteFolderOutcome {
            folders_moved,
            documents_moved,
        })
    }


    /// Folders that have been renamed, reparented, created, or deleted
    /// locally and not yet pushed to the device. The push engine
    /// routes each entry by `FolderPushOp` kind: `Upsert` uploads the
    /// folder's `<uuid>.metadata` file (rename / reparent / create),
    /// while `Delete` SFTP-removes the folder's files outright. The
    /// latter shape matters because pushing a metadata file with
    /// `deleted: true` only sends the folder to xochitl's Trash on
    /// the device — surprising users who expected "Delete folder" to
    /// actually delete it. Hard-deleting the files removes the
    /// folder in one step.
    pub fn list_pending_folder_pushes(&self) -> Result<Vec<FolderPushOp>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT folder_id, metadata_json, deleted_locally FROM folders \
             WHERE pending_push = 1 ORDER BY folder_id",
        )?;
        let rows: Vec<FolderPushOp> = stmt
            .query_map([], |r| {
                let folder_id: String = r.get(0)?;
                let metadata_json: String = r.get(1)?;
                let deleted: i64 = r.get(2)?;
                Ok(if deleted == 1 {
                    FolderPushOp::Delete { folder_id }
                } else {
                    FolderPushOp::Upsert {
                        folder_id,
                        metadata_json,
                    }
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }


    /// Mark a folder's local metadata as flushed to the device. Called
    /// by the push engine after a successful upload (or hard-delete).
    ///
    /// For a tombstoned folder (`deleted_locally = 1`) the successful
    /// push means xochitl no longer has the folder's files — the
    /// row's job is done, so we drop it entirely.
    ///
    /// For a regular pending row (rename / reparent / fresh create)
    /// we clear `pending_push` AND refresh `last_synced_metadata_json`
    /// from the just-pushed `metadata_json`. The latter matters for
    /// the Revert feature: after a successful push the device's
    /// state now equals our local state, so the snapshot has to
    /// move forward too — otherwise the next local edit would still
    /// roll back to the pre-push state when reverted.
    pub fn mark_folder_pushed(&self, folder_id: &str, kind: FolderPushKind) -> Result<()> {
        // Route by the op that was actually pushed, NOT by the row's
        // current `deleted_locally`. Reading `deleted_locally` here
        // would race a concurrent `delete_folder`: imagine the push
        // engine ships an Upsert for folder F to the device, then the
        // user clicks "Delete" on F. `delete_folder` flips
        // `deleted_locally` to 1; if mark_folder_pushed then read that
        // flag it would `DELETE FROM folders`, dropping the queued
        // tombstone before any sync ever shipped it. The result: F
        // is gone from the library but still on the tablet, with no
        // pending-push row to ever clean it up.
        //
        // By taking `kind` from the caller (which holds the queue
        // snapshot from `list_pending_folder_pushes`) the routing is
        // immune to concurrent mutation between snapshot and commit.
        let _write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;
        match kind {
            FolderPushKind::Delete => {
                // Tombstone shipped. The row's job is done.
                tx.execute(
                    "DELETE FROM folders WHERE folder_id = ?1",
                    params![folder_id],
                )?;
            }
            FolderPushKind::Upsert => {
                // Clear pending_push and advance the snapshot — but
                // gate on `deleted_locally = 0`. If a concurrent
                // `delete_folder` flipped `deleted_locally` to 1
                // between the queue snapshot and now, the next push
                // must still see this row in `list_pending_folder_pushes`
                // so the Delete actually reaches the device. Leaving
                // pending_push = 1 in that case is the right move.
                tx.execute(
                    "UPDATE folders SET pending_push = 0, last_synced_metadata_json = metadata_json \
                     WHERE folder_id = ?1 AND deleted_locally = 0",
                    params![folder_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}
