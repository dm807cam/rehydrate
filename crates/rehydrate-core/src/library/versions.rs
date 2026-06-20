//! Library API for the per-document version log: record / list history /
//! restore / set note. The internal `record_version_in_tx` is the
//! transactional core that other modules (documents, archive, derived,
//! import) reach into via `record_metadata_change` / direct calls.

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

    /// Append a new version for the document described by `manifest`. If the
    /// manifest's hash equals the document's current_manifest, this is a no-op
    /// and the existing version_id is returned with `unchanged=true`.
    pub fn record_version(&self, manifest: &Manifest, source: Source) -> Result<RecordOutcome> {
        let _write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());

        let canonical = manifest.canonical_json()?;
        let manifest_hash = Sha256Hex::from_bytes(&canonical);

        // Persist the manifest itself as a blob (idempotent — orphan
        // collected by GC after the grace if we never commit).
        self.blobs.put_bytes(&canonical)?;

        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        // Archive-vs-pull race guard (issue #31). If the user archived
        // this doc while we were fetching its bytes, an unconditional
        // INSERT ... ON CONFLICT UPDATE in record_version_in_tx would
        // resurrect the row into `documents` while `archived_documents`
        // still holds the deleted=true manifest — `list_pushable_documents`
        // then ships BOTH, last-write-wins on the device drops the doc
        // the user never asked to delete. The sister `record_metadata_change`
        // path has the matching guard at the top of its flow; surfacing
        // a typed error here lets the sync engine treat the doc as
        // per-call skipped without aborting the rest of the pull.
        // unarchive_document deliberately bypasses this gate by going
        // through record_metadata_change (which calls record_version_in_tx
        // directly with PostAction::Unarchive).
        let archived: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM archived_documents WHERE document_id = ?1)",
            params![manifest.document_id],
            |r| r.get(0),
        )?;
        if archived {
            return Err(CoreError::DocumentArchived(manifest.document_id.clone()));
        }

        let outcome = self.record_version_in_tx(&tx, manifest, &manifest_hash, source)?;
        tx.commit()?;
        Ok(outcome)
    }


    /// Pure-SQL part of `record_version`. The caller is expected to have
    /// already written the manifest blob to disk and to be holding both
    /// `write_lock` and a SQLite transaction.
    ///
    /// Factoring this out lets `record_metadata_change`,
    /// `archive_document`, and `unarchive_document` compose multiple
    /// row mutations into a single transaction — without it, each
    /// caller had to commit a partial state and then take a second
    /// lock + tx to finish, which left a window for crash-recovery
    /// and concurrent-edit races (audit findings C1 + C2).
    pub(super) fn record_version_in_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        manifest: &Manifest,
        manifest_hash: &Sha256Hex,
        source: Source,
    ) -> Result<RecordOutcome> {
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();

        // Already the current version?
        let current: Option<(String, i64)> = tx
            .query_row(
                "SELECT current_manifest, current_version_id FROM documents WHERE document_id = ?1",
                params![manifest.document_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;

        if let Some((cur_hash, cur_id)) = &current {
            if cur_hash == manifest_hash.as_str() {
                // Same manifest as current — only the device-side known
                // state advances if this is a pull. Restored/imported
                // re-records change nothing observable, so skip updating
                // last_seen_manifest here.
                if matches!(source, Source::Pulled) {
                    tx.execute(
                        "UPDATE sync_state SET last_seen_manifest = ?1, last_synced_at = ?2 \
                         WHERE document_id = ?3",
                        params![manifest_hash.as_str(), now, manifest.document_id],
                    )?;
                }
                return Ok(RecordOutcome {
                    version_id: *cur_id,
                    manifest_hash: manifest_hash.clone(),
                    unchanged: true,
                });
            }
        }

        let parent_version_id = current.as_ref().map(|(_, id)| *id);

        tx.execute(
            "INSERT INTO versions(document_id, manifest_hash, parent_version_id, observed_at, source, note) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
            params![
                manifest.document_id,
                manifest_hash.as_str(),
                parent_version_id,
                now,
                source.as_str()
            ],
        )?;
        let version_id = tx.last_insert_rowid();

        tx.execute(
            "INSERT INTO documents(document_id, current_manifest, current_version_id) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(document_id) DO UPDATE SET \
                 current_manifest = excluded.current_manifest, \
                 current_version_id = excluded.current_version_id",
            params![manifest.document_id, manifest_hash.as_str(), version_id],
        )?;

        // Populate blob_refs (manifest itself + every file).
        tx.execute(
            "INSERT OR IGNORE INTO blob_refs(blob_hash, manifest_hash) VALUES (?1, ?1)",
            params![manifest_hash.as_str()],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO blob_refs(blob_hash, manifest_hash) VALUES (?1, ?2)",
            )?;
            for f in &manifest.files {
                stmt.execute(params![f.sha256.as_str(), manifest_hash.as_str()])?;
            }
        }

        // `last_seen_manifest` tracks "what's known to be on the device".
        // Only `Source::Pulled` may advance it — restored/imported manifests
        // are library-side changes that should trigger a future push.
        if matches!(source, Source::Pulled) {
            tx.execute(
                "INSERT INTO sync_state(document_id, last_seen_manifest, last_synced_at) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT(document_id) DO UPDATE SET \
                     last_seen_manifest = excluded.last_seen_manifest, \
                     last_synced_at = excluded.last_synced_at",
                params![manifest.document_id, manifest_hash.as_str(), now],
            )?;
        } else {
            // Ensure the row exists so future planners have somewhere to
            // read from, but leave last_seen_manifest alone.
            tx.execute(
                "INSERT INTO sync_state(document_id) VALUES (?1) \
                 ON CONFLICT(document_id) DO NOTHING",
                params![manifest.document_id],
            )?;
        }

        Ok(RecordOutcome {
            version_id,
            manifest_hash: manifest_hash.clone(),
            unchanged: false,
        })
    }


    pub fn get_history(&self, document_id: &str) -> Result<Vec<VersionEntry>> {
        // Pull raw rows first; defer hash validation so a single corrupt
        // row produces a typed `CoreError::Corrupt` instead of getting
        // wrapped through rusqlite's error type or — worse, before
        // the H4 fix — silently fabricated.
        type RawRow = (
            i64,
            String,
            String,
            Option<i64>,
            String,
            String,
            Option<String>,
        );
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT id, document_id, manifest_hash, parent_version_id, observed_at, source, note \
             FROM versions WHERE document_id = ?1 ORDER BY id ASC",
        )?;
        let raw: Vec<RawRow> = stmt
            .query_map(params![document_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        drop(conn);

        let mut rows: Vec<VersionEntry> = Vec::with_capacity(raw.len());
        for (id, doc_id, manifest_hex, parent, observed_at, source_str, note) in raw {
            let manifest_hash =
                Sha256Hex::from_hex(&manifest_hex).ok_or_else(|| CoreError::Corrupt {
                    path: self.paths.db.display().to_string(),
                    reason: format!("bad manifest hash for version {id}"),
                })?;
            rows.push(VersionEntry {
                id,
                document_id: doc_id,
                manifest_hash,
                parent_version_id: parent,
                observed_at,
                source: Source::parse(&source_str).unwrap_or(Source::Pulled),
                note,
                total_size_bytes: None,
                file_count: None,
            });
        }

        // Populate size + file count by reading each manifest blob. Cheap
        // for typical libraries (a few hundred bytes per manifest); if it
        // ever shows up as hot we can cache these values in the schema.
        for entry in &mut rows {
            if let Ok(bytes) = self.blobs.read_to_vec(&entry.manifest_hash) {
                if let Ok(m) = Manifest::from_canonical_json(&bytes) {
                    entry.total_size_bytes = Some(m.files.iter().map(|f| f.size).sum());
                    entry.file_count = Some(m.files.len());
                }
            }
        }
        Ok(rows)
    }


    /// Look up a single version by id. Returns `Err(NotFound)` if no row
    /// matches. Used by `export_version` and the history UI.
    pub fn get_version(&self, version_id: VersionId) -> Result<VersionEntry> {
        struct Row {
            document_id: String,
            manifest_hex: String,
            parent_version_id: Option<i64>,
            observed_at: String,
            source_str: String,
            note: Option<String>,
        }
        let row: Option<Row> = self
            .db
            .lock()
            .query_row(
                "SELECT document_id, manifest_hash, parent_version_id, observed_at, source, note \
                 FROM versions WHERE id = ?1",
                params![version_id],
                |r| {
                    Ok(Row {
                        document_id: r.get(0)?,
                        manifest_hex: r.get(1)?,
                        parent_version_id: r.get(2)?,
                        observed_at: r.get(3)?,
                        source_str: r.get(4)?,
                        note: r.get(5)?,
                    })
                },
            )
            .optional()?;
        let Row {
            document_id,
            manifest_hex,
            parent_version_id,
            observed_at,
            source_str,
            note,
        } = row.ok_or_else(|| CoreError::NotFound(format!("version {version_id}")))?;
        let manifest_hash =
            Sha256Hex::from_hex(&manifest_hex).ok_or_else(|| CoreError::Corrupt {
                path: self.paths.db.display().to_string(),
                reason: format!("bad manifest hash for version {version_id}"),
            })?;
        let mut entry = VersionEntry {
            id: version_id,
            document_id,
            manifest_hash: manifest_hash.clone(),
            parent_version_id,
            observed_at,
            source: Source::parse(&source_str).unwrap_or(Source::Pulled),
            note,
            total_size_bytes: None,
            file_count: None,
        };
        if let Ok(bytes) = self.blobs.read_to_vec(&manifest_hash) {
            if let Ok(m) = Manifest::from_canonical_json(&bytes) {
                entry.total_size_bytes = Some(m.files.iter().map(|f| f.size).sum());
                entry.file_count = Some(m.files.len());
            }
        }
        Ok(entry)
    }


    /// Total versions across all documents.
    pub fn version_count(&self) -> Result<i64> {
        Ok(self
            .db
            .lock()
            .query_row("SELECT count(*) FROM versions", [], |r| r.get(0))?)
    }


    /// Make `version_id` the document's current version by re-recording its
    /// manifest with `Source::Restored`. The previous current version is
    /// preserved in the log via `parent_version_id`. If `version_id` is
    /// already current, this is a no-op (`unchanged: true`).
    pub fn restore_version(&self, version_id: VersionId) -> Result<RecordOutcome> {
        let entry = self.get_version(version_id)?;
        let bytes = self.read_blob(&entry.manifest_hash)?;
        let manifest = Manifest::from_canonical_json(&bytes)?;
        self.record_version(&manifest, Source::Restored)
    }


    /// Update the free-form note attached to a version. Pass `None` to clear.
    pub fn set_version_note(&self, version_id: VersionId, note: Option<&str>) -> Result<()> {
        let n = self.db.lock().execute(
            "UPDATE versions SET note = ?1 WHERE id = ?2",
            params![note, version_id],
        )?;
        if n == 0 {
            return Err(CoreError::NotFound(format!("version {version_id}")));
        }
        Ok(())
    }


    /// Document IDs the library believes were on the device at last sync —
    /// i.e. they have a non-null `last_seen_manifest` and aren't already in
    /// the archive. Used by the pull engine to detect device-side deletions.
    pub fn previously_synced_ids(&self) -> Result<Vec<String>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT s.document_id FROM sync_state s \
             WHERE s.last_seen_manifest IS NOT NULL \
             AND s.document_id NOT IN (SELECT document_id FROM archived_documents)",
        )?;
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
}
