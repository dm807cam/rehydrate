//! Library API for the archive lifecycle and the device-deletion queue:
//! archive_document, unarchive_document, purge_archived_document,
//! list_archived, list_device_deletion_queue, dequeue_device_deletion.

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

    /// Move a document from the live `documents` table into the archive.
    /// Versions stay intact so a later `unarchive_document` can restore the
    /// listing exactly. If the document is already archived this is a
    /// no-op; if it doesn't exist we return `NotFound`.
    pub fn archive_document(&self, document_id: &str, reason: ArchiveReason) -> Result<()> {
        // Idempotent: re-archiving an already-archived doc is a no-op so
        // the device-deletion detector (which calls this in a loop) and
        // the UI button can both invoke it freely.
        if self.is_archived(document_id)? {
            return Ok(());
        }

        // Capture the doc's pre-archive parent + visibleName + doc_type
        // so a later unarchive can restore it where the user had it.
        // Reading before record_metadata_change is fine — the mutation
        // flips parent to "trash", and record_metadata_change re-reads
        // under write_lock + tx so the rename/move race window is
        // closed even though this read runs first.
        let (original_parent, visible_name, doc_type) = {
            let conn = self.db.lock();
            let h: String = conn
                .query_row(
                    "SELECT current_manifest FROM documents WHERE document_id = ?1",
                    params![document_id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| CoreError::NotFound(format!("document {document_id}")))?;
            drop(conn);
            let hash = Sha256Hex::from_hex(&h).ok_or_else(|| CoreError::Corrupt {
                path: self.paths.db.display().to_string(),
                reason: format!("bad manifest hash for {document_id}"),
            })?;
            let bytes = self.blobs.read_to_vec(&hash)?;
            let m = Manifest::from_canonical_json(&bytes)?;
            (m.parent.clone(), m.visible_name, m.doc_type)
        };

        // One atomic transaction: write the deleted=true version AND
        // move documents → archived_documents in the same tx (audit
        // fix C2). A crash mid-way used to leave the doc with a
        // deleted manifest but never archived; now it's all-or-nothing.
        self.record_metadata_change(
            document_id,
            |map| {
                map.insert("deleted".into(), serde_json::Value::Bool(true));
                map.insert("parent".into(), serde_json::Value::String("trash".into()));
                Ok(())
            },
            PostAction::Archive {
                reason,
                visible_name: &visible_name,
                doc_type: &doc_type,
                original_parent: original_parent.as_deref(),
            },
        )?;
        Ok(())
    }


    /// Restore an archived document to the live listing using the manifest
    /// it had at archive time. Returns `NotFound` if the id isn't in the
    /// archive.
    pub fn unarchive_document(&self, document_id: &str) -> Result<DocumentSummary> {
        // Read the archive entry first — it tells us the original parent
        // to restore the doc to. record_metadata_change re-reads under
        // write_lock + tx so any concurrent edit lands either before
        // or after this whole flow.
        let row: (String, String, Option<String>) = {
            let conn = self.db.lock();
            conn.query_row(
                "SELECT visible_name, doc_type, parent \
                 FROM archived_documents WHERE document_id = ?1",
                params![document_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("archived document {document_id}")))?
        };
        let (visible_name, doc_type, original_parent) = row;
        // Dangling-parent fallback. The archive entry remembers the
        // parent folder uuid the document had at archive time, but
        // the user may have deleted that folder in the meantime
        // (`delete_folder` doesn't touch archived docs, by design).
        // Without this check the unarchived doc lands with a parent
        // pointing at a folder that no longer exists, becoming a
        // sidebar orphan that's only visible at root. Resolve by
        // probing the folders table and falling back to root if the
        // recorded parent is gone.
        let original_parent: Option<String> = match original_parent {
            Some(p) if !p.is_empty() => {
                let conn = self.db.lock();
                let parent_exists: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM folders \
                     WHERE folder_id = ?1 AND deleted_locally = 0",
                    params![p],
                    |r| r.get(0),
                )?;
                if parent_exists == 1 {
                    Some(p)
                } else {
                    None
                }
            }
            _ => None,
        };
        let parent_for_meta = original_parent.clone().unwrap_or_default();

        // Single tx writes the deleted=false version AND deletes the
        // archived_documents row (record_version_in_tx already inserts
        // into documents). Audit fix C2.
        let outcome = self.record_metadata_change(
            document_id,
            |map| {
                map.insert("deleted".into(), serde_json::Value::Bool(false));
                map.insert(
                    "parent".into(),
                    serde_json::Value::String(parent_for_meta.clone()),
                );
                Ok(())
            },
            PostAction::Unarchive,
        )?;

        // Read observed_at for the freshly-recorded version. No lock —
        // it's a read-only query.
        let observed_at: String = {
            let conn = self.db.lock();
            conn.query_row(
                "SELECT observed_at FROM versions WHERE id = ?1",
                params![outcome.version_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default()
        };

        Ok(DocumentSummary {
            document_id: document_id.to_string(),
            visible_name,
            doc_type,
            current_manifest: outcome.manifest_hash,
            current_version_id: outcome.version_id,
            last_observed_at: observed_at,
            parent: original_parent,
            size_bytes: 0,
            page_count: None,
            has_unpushed_changes: true,
        })
    }


    /// Permanently delete an archived document: drop the archive row, the
    /// version log entries, and any sync_state. Blobs that are no longer
    /// referenced by any version become orphans and will be reclaimed by
    /// the next `garbage_collect`.
    pub fn purge_archived_document(&self, document_id: &str) -> Result<()> {
        let _write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        let exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM archived_documents WHERE document_id = ?1",
                params![document_id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(CoreError::NotFound(format!(
                "archived document {document_id}"
            )));
        }

        // If the document was ever pushed to the device (last_seen_manifest
        // is non-NULL), queue a hard-delete so the pull engine removes it
        // from the tablet on next sync instead of re-downloading it.
        let was_synced: bool = tx
            .query_row(
                "SELECT last_seen_manifest IS NOT NULL \
                 FROM sync_state WHERE document_id = ?1",
                params![document_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);

        tx.execute(
            "DELETE FROM archived_documents WHERE document_id = ?1",
            params![document_id],
        )?;
        tx.execute(
            "DELETE FROM versions WHERE document_id = ?1",
            params![document_id],
        )?;
        tx.execute(
            "DELETE FROM sync_state WHERE document_id = ?1",
            params![document_id],
        )?;

        if was_synced {
            tx.execute(
                "INSERT OR IGNORE INTO device_deletion_queue(document_id) VALUES (?1)",
                params![document_id],
            )?;
        }

        tx.commit()?;
        Ok(())
    }


    /// Return all document IDs currently queued for hard-deletion from the
    /// device. Called by the pull engine before processing the device listing.
    pub fn list_device_deletion_queue(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.db.lock();
        let mut stmt =
            conn.prepare("SELECT document_id FROM device_deletion_queue")?;
        let rows: std::collections::HashSet<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }


    /// Remove a document from the device-deletion queue after the tablet-side
    /// hard-delete has been confirmed (or is known to be unnecessary).
    pub fn dequeue_device_deletion(&self, document_id: &str) -> Result<()> {
        self.db.lock().execute(
            "DELETE FROM device_deletion_queue WHERE document_id = ?1",
            params![document_id],
        )?;
        Ok(())
    }


    /// Return the current archive contents, newest first.
    pub fn list_archived(&self) -> Result<Vec<ArchivedDocument>> {
        type RawRow = (
            String,
            String,
            String,
            Option<String>,
            String,
            i64,
            String,
            String,
        );
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT document_id, visible_name, doc_type, parent, \
                    manifest_hash, version_id, reason, archived_at \
             FROM archived_documents \
             ORDER BY archived_at DESC, document_id ASC",
        )?;
        let raw: Vec<RawRow> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        drop(conn);

        let mut rows = Vec::with_capacity(raw.len());
        for (
            document_id,
            visible_name,
            doc_type,
            parent,
            manifest_hex,
            version_id,
            reason_str,
            archived_at,
        ) in raw
        {
            let manifest_hash =
                Sha256Hex::from_hex(&manifest_hex).ok_or_else(|| CoreError::Corrupt {
                    path: self.paths.db.display().to_string(),
                    reason: format!("bad manifest hash for archived document {document_id}"),
                })?;
            rows.push(ArchivedDocument {
                document_id,
                visible_name,
                doc_type,
                parent,
                manifest_hash,
                version_id,
                reason: ArchiveReason::parse(&reason_str).unwrap_or(ArchiveReason::Local),
                archived_at,
            });
        }
        Ok(rows)
    }
}
