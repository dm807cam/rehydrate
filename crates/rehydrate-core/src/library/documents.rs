//! Library API for document-level operations: list, rename, move,
//! metadata-change bookkeeping (`record_metadata_change` — the
//! pull-vs-local-edit conflict guard), last-seen tracking,
//! is_archived / list_pushable_documents.

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

    pub fn list_documents(&self) -> Result<Vec<DocumentSummary>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT d.document_id, d.current_manifest, d.current_version_id, v.observed_at, \
                    s.last_seen_manifest \
             FROM documents d \
             JOIN versions v ON v.id = d.current_version_id \
             LEFT JOIN sync_state s ON s.document_id = d.document_id \
             ORDER BY d.document_id",
        )?;
        let rows: Vec<(String, String, i64, String, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        drop(conn);

        let mut out = Vec::with_capacity(rows.len());
        for (document_id, manifest_hex, version_id, observed_at, last_seen_manifest) in rows {
            let hash = Sha256Hex::from_hex(&manifest_hex).ok_or_else(|| CoreError::Corrupt {
                path: self.paths.db.display().to_string(),
                reason: format!("bad manifest hash for {document_id}"),
            })?;
            let manifest_bytes = self.blobs.read_to_vec(&hash)?;
            let manifest = Manifest::from_canonical_json(&manifest_bytes)?;
            let size_bytes: u64 = manifest.files.iter().map(|f| f.size).sum();
            let page_count = manifest
                .content_meta
                .get("pageCount")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok());
            // Document has unpushed changes if its current manifest
            // doesn't match what the device last received. Never-synced
            // (last_seen_manifest IS NULL) also counts.
            let has_unpushed_changes = match &last_seen_manifest {
                Some(ls) => ls != &manifest_hex,
                None => true,
            };
            out.push(DocumentSummary {
                document_id,
                visible_name: manifest.visible_name,
                doc_type: manifest.doc_type,
                current_manifest: hash,
                current_version_id: version_id,
                last_observed_at: observed_at,
                parent: manifest.parent,
                size_bytes,
                page_count,
                has_unpushed_changes,
            });
        }
        Ok(out)
    }


    /// Record a new version of a document with its `.metadata` blob
    /// rewritten in place by `mutate`. The new metadata is also pushed into
    /// `manifest.metadata` and `manifest.parent` so a future planner can
    /// see the change without re-reading the blob.
    ///
    /// Used by every "library-side edit" path: move-to-folder, archive
    /// (deleted=true), unarchive (deleted=false). Bumps the device-facing
    /// `lastModified` and `modified` flags so the tablet treats the file
    /// as freshly changed.
    ///
    /// The whole read → mutate → write sequence runs under
    /// `write_lock` plus a single SQLite transaction. `post_action`
    /// lets archive / unarchive piggy-back their `documents` ↔
    /// `archived_documents` move on the same transaction (audit
    /// fixes C1 + C2). Without that coupling, two callers could
    /// observe the same parent manifest and silently overwrite each
    /// other, and a crash between the version-record commit and the
    /// archive move could leave a row in a deleted-but-not-archived
    /// limbo.
    pub(super) fn record_metadata_change<F>(
        &self,
        document_id: &str,
        mutate: F,
        post_action: PostAction<'_>,
    ) -> Result<RecordOutcome>
    where
        F: FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<()>,
    {
        // Take the write lock for the WHOLE flow — read, mutate, write —
        // so concurrent rename/move/archive callers serialise properly.
        let _write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());

        // Brief db lock to resolve the current manifest hash. Only
        // `PostAction::Unarchive` is allowed to fall back to the
        // archive table — every other caller (move_document,
        // rename_document, record_derived_artefact, …) must treat an
        // archived id as NotFound. Otherwise record_version_in_tx's
        // unconditional `INSERT INTO documents ON CONFLICT UPDATE`
        // would resurrect the archived row into the live listing,
        // leaving the doc in both `documents` and `archived_documents`
        // simultaneously. We must drop this lock before filesystem I/O
        // because the connection is held by transactional code below.
        let manifest_hex: String = {
            let conn = self.db.lock();
            let live: Option<String> = conn
                .query_row(
                    "SELECT current_manifest FROM documents WHERE document_id = ?1",
                    params![document_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(h) = live {
                h
            } else if matches!(post_action, PostAction::Unarchive) {
                conn.query_row(
                    "SELECT manifest_hash FROM archived_documents WHERE document_id = ?1",
                    params![document_id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| CoreError::NotFound(format!("document {document_id}")))?
            } else {
                return Err(CoreError::NotFound(format!("document {document_id}")));
            }
        };

        let hash = Sha256Hex::from_hex(&manifest_hex).ok_or_else(|| CoreError::Corrupt {
            path: self.paths.db.display().to_string(),
            reason: format!("bad manifest hash for {document_id}"),
        })?;
        let manifest_bytes = self.blobs.read_to_vec(&hash)?;
        let mut manifest = Manifest::from_canonical_json(&manifest_bytes)?;

        let meta_idx = manifest
            .files
            .iter()
            .position(|f| f.path.ends_with(".metadata"))
            .ok_or_else(|| CoreError::Corrupt {
                path: "<manifest>".into(),
                reason: format!("document {document_id} has no .metadata file"),
            })?;
        let meta_bytes = self.blobs.read_to_vec(&manifest.files[meta_idx].sha256)?;
        let mut meta_value: serde_json::Value = serde_json::from_slice(&meta_bytes)?;
        let map = meta_value
            .as_object_mut()
            .ok_or_else(|| CoreError::Corrupt {
                path: "<manifest>".into(),
                reason: format!("metadata for {document_id} is not a JSON object"),
            })?;

        mutate(map)?;

        // Mark the file as changed locally so the tablet's xochitl picks
        // up the new metadata on next sync. Empty defaults are safe — if
        // these keys were missing they'll simply be set.
        let now_ms = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
            .unwrap_or(i64::MAX);
        map.insert(
            "lastModified".into(),
            serde_json::Value::String(now_ms.to_string()),
        );
        map.insert("modified".into(), serde_json::Value::Bool(true));
        map.insert("metadatamodified".into(), serde_json::Value::Bool(true));
        map.insert("synced".into(), serde_json::Value::Bool(false));

        let new_meta_bytes = serde_json::to_vec_pretty(&meta_value)?;
        let put = self.put_blob(&new_meta_bytes)?;
        manifest.files[meta_idx].sha256 = put.hash;
        manifest.files[meta_idx].size = put.size;
        manifest.metadata = meta_value.clone();

        // Reflect the post-mutation parent + visible name on the
        // manifest's top-level fields so SQL queries that read them
        // (sidebar filtering, archive entry display) match what's in
        // the metadata blob.
        manifest.parent = match meta_value.get("parent").and_then(|v| v.as_str()) {
            None | Some("") => None,
            Some(p) => Some(p.to_string()),
        };
        if let Some(name) = meta_value.get("visibleName").and_then(|v| v.as_str()) {
            manifest.visible_name = name.to_string();
        }

        // Write the new manifest blob (idempotent; GC reclaims if we
        // fail to commit below) and run the SQL atomically.
        let canonical = manifest.canonical_json()?;
        let manifest_hash = Sha256Hex::from_bytes(&canonical);
        self.blobs.put_bytes(&canonical)?;

        let mut conn = self.db.lock();
        let tx = conn.transaction()?;
        let outcome =
            self.record_version_in_tx(&tx, &manifest, &manifest_hash, Source::Restored)?;

        match post_action {
            PostAction::None => {}
            PostAction::Archive {
                reason,
                visible_name,
                doc_type,
                original_parent,
            } => {
                let now = OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default();
                tx.execute(
                    "INSERT INTO archived_documents \
                        (document_id, visible_name, doc_type, parent, manifest_hash, \
                         version_id, reason, archived_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                     ON CONFLICT(document_id) DO UPDATE SET \
                        manifest_hash = excluded.manifest_hash, \
                        version_id = excluded.version_id, \
                        reason = excluded.reason, \
                        archived_at = excluded.archived_at",
                    params![
                        document_id,
                        visible_name,
                        doc_type,
                        original_parent,
                        outcome.manifest_hash.as_str(),
                        outcome.version_id,
                        reason.as_str(),
                        now,
                    ],
                )?;
                tx.execute(
                    "DELETE FROM documents WHERE document_id = ?1",
                    params![document_id],
                )?;
            }
            PostAction::Unarchive => {
                tx.execute(
                    "DELETE FROM archived_documents WHERE document_id = ?1",
                    params![document_id],
                )?;
            }
        }

        tx.commit()?;
        Ok(outcome)
    }


    /// Rename a document. Writes the new title into `.metadata`'s
    /// `visibleName`, records a new version, and flags the doc as having
    /// unpushed changes — the next sync sends the new metadata to the
    /// tablet which then displays the new name. Empty / whitespace-only
    /// names are rejected so the tablet never ends up with a blank title.
    pub fn rename_document(&self, document_id: &str, new_name: &str) -> Result<RecordOutcome> {
        let trimmed = new_name.trim().to_string();
        if trimmed.is_empty() {
            return Err(CoreError::InvalidArgument(
                "document name must not be empty".into(),
            ));
        }
        self.record_metadata_change(
            document_id,
            |map| {
                map.insert(
                    "visibleName".into(),
                    serde_json::Value::String(trimmed.clone()),
                );
                Ok(())
            },
            PostAction::None,
        )
    }


    /// Move a live document into a different folder (`new_parent` =
    /// `Some(folder_uuid)`) or to the root (`None`). Records a new version
    /// so the device picks up the move on next push.
    pub fn move_document(
        &self,
        document_id: &str,
        new_parent: Option<&str>,
    ) -> Result<RecordOutcome> {
        let parent = new_parent.unwrap_or("").to_string();
        self.record_metadata_change(
            document_id,
            |map| {
                map.insert("parent".into(), serde_json::Value::String(parent));
                Ok(())
            },
            PostAction::None,
        )
    }


    /// Look up the cached classification hint for one document. Used by the
    /// sync engine's `plan_pull` to decide between `New`, `Changed`, and
    /// `Unchanged` without rehashing.
    pub fn last_seen(&self, document_id: &str) -> Result<Option<(Option<String>, Option<String>)>> {
        Ok(self
            .db
            .lock()
            .query_row(
                "SELECT device_mtime_hint, last_seen_manifest FROM sync_state WHERE document_id = ?1",
                params![document_id],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?)
    }


    /// Update the device-side mtime hint after a successful download. Cheap
    /// fast-path classifier for next time `plan_pull` runs.
    pub fn update_mtime_hint(&self, document_id: &str, hint: &str) -> Result<()> {
        self.db.lock().execute(
            "INSERT INTO sync_state(document_id, device_mtime_hint) VALUES (?1, ?2) \
             ON CONFLICT(document_id) DO UPDATE SET device_mtime_hint = excluded.device_mtime_hint",
            params![document_id, hint],
        )?;
        Ok(())
    }


    /// Mark the manifest as the latest one known to be on the device. Called
    /// after a successful push so the next `plan_pull` correctly classifies
    /// the document as Unchanged (assuming the device hasn't moved on).
    pub fn update_last_seen_manifest(&self, document_id: &str, manifest_hex: &str) -> Result<()> {
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        self.db.lock().execute(
            "INSERT INTO sync_state(document_id, last_seen_manifest, last_synced_at) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(document_id) DO UPDATE SET \
                 last_seen_manifest = excluded.last_seen_manifest, \
                 last_synced_at = excluded.last_synced_at",
            params![document_id, manifest_hex, now],
        )?;
        Ok(())
    }


    /// Test whether a document is currently archived. Used by the sync
    /// engine to skip re-creating a live entry for a document the user has
    /// already chosen to archive.
    pub fn is_archived(&self, document_id: &str) -> Result<bool> {
        let conn = self.db.lock();
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM archived_documents WHERE document_id = ?1",
            params![document_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }


    /// Both live and archived documents that the push engine needs to
    /// consider. Archived docs are returned as DocumentSummary entries
    /// pointing at their *post-archive* manifest (deleted=true) so the
    /// next push uploads the deletion to the device. The caller must not
    /// rely on these being present in `list_documents` — that one is for
    /// the user-facing live view.
    pub fn list_pushable_documents(&self) -> Result<Vec<DocumentSummary>> {
        let mut out = self.list_documents()?;
        let archived = self.list_archived()?;
        for a in archived {
            // observed_at is fetched from the version row so plan_push and
            // history views agree on timestamps.
            let observed_at: String = self
                .db
                .lock()
                .query_row(
                    "SELECT observed_at FROM versions WHERE id = ?1",
                    params![a.version_id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or_default();
            out.push(DocumentSummary {
                document_id: a.document_id,
                visible_name: a.visible_name,
                doc_type: a.doc_type,
                current_manifest: a.manifest_hash,
                current_version_id: a.version_id,
                last_observed_at: observed_at,
                // The archive table stores the *original* parent; the
                // manifest itself has parent="trash". For push purposes
                // the manifest blob is what gets uploaded, so the parent
                // value here is informational only.
                parent: Some("trash".to_string()),
                size_bytes: 0,
                page_count: None,
                has_unpushed_changes: true,
            });
        }
        Ok(out)
    }
}
