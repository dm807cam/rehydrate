//! Library API for derived artefacts: per-version side-files (e.g.
//! `ocr/transcript.md`) attached to a version without re-pulling the
//! whole tree from the device.

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

    /// Attach a library-side derived artefact (e.g. an OCR transcript)
    /// to the document's current manifest. Creates a fresh version
    /// containing `<path>` with `derived: true`, replacing any prior
    /// entry at the same path. The blob bytes are content-addressed and
    /// committed to the blob store before the SQL transaction; any
    /// pre-existing file with the same path is removed from the
    /// manifest (its blob will be reclaimed by GC if unreferenced).
    ///
    /// The whole read → mutate → write sequence runs under
    /// `write_lock` plus a single SQLite tx, same shape as
    /// `record_metadata_change`, so concurrent edits don't lose
    /// the transcript.
    pub fn record_derived_artefact(
        &self,
        document_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> Result<RecordOutcome> {
        if !path.starts_with("ocr/") {
            // Defence: keep the derived namespace tightly scoped so a
            // future caller can't shadow legitimate device files via
            // this back-door.
            return Err(CoreError::InvalidArgument(format!(
                "derived artefact path must live under `ocr/`, got {path:?}"
            )));
        }

        let _write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());

        let manifest_hex: String = {
            let conn = self.db.lock();
            conn.query_row(
                "SELECT current_manifest FROM documents WHERE document_id = ?1",
                params![document_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("document {document_id}")))?
        };
        let hash = Sha256Hex::from_hex(&manifest_hex).ok_or_else(|| CoreError::Corrupt {
            path: self.paths.db.display().to_string(),
            reason: format!("bad manifest hash for {document_id}"),
        })?;
        let manifest_bytes = self.blobs.read_to_vec(&hash)?;
        let mut manifest = Manifest::from_canonical_json(&manifest_bytes)?;

        let put = self.put_blob(bytes)?;
        let new_file = ManifestFile {
            path: path.to_string(),
            sha256: put.hash,
            size: put.size,
            mode: 0o644,
            derived: true,
        };
        if let Some(existing) = manifest.files.iter_mut().find(|f| f.path == path) {
            *existing = new_file;
        } else {
            manifest.files.push(new_file);
        }

        let canonical = manifest.canonical_json()?;
        let manifest_hash = Sha256Hex::from_bytes(&canonical);
        self.blobs.put_bytes(&canonical)?;

        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        // Snapshot the prior sync_state row before record_version_in_tx
        // potentially writes one. We use this to decide whether to
        // auto-advance `last_seen_manifest` (see below).
        let prior_last_seen: Option<String> = tx
            .query_row(
                "SELECT last_seen_manifest FROM sync_state WHERE document_id = ?1",
                params![document_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        let outcome =
            self.record_version_in_tx(&tx, &manifest, &manifest_hash, Source::Imported)?;

        // Derived artefacts (OCR transcripts) live entirely on the
        // PC — push.rs filters them out of the upload payload. Without
        // this clause, the unchanged `current_manifest` → new
        // `current_manifest` delta still flips `has_unpushed_changes`
        // to true and trips `plan_push` into queuing a no-op
        // re-upload of every other file.
        //
        // Auto-advance `last_seen_manifest` to the new hash IF the
        // device was already in sync with the prior manifest. If the
        // user had unpushed changes (rename, move, archive…), leave
        // `last_seen` alone so the next push still ships them — the
        // derived file gets filtered out of that push regardless.
        if prior_last_seen.as_deref() == Some(manifest_hex.as_str()) {
            let now = OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default();
            tx.execute(
                "UPDATE sync_state SET last_seen_manifest = ?1, last_synced_at = ?2 \
                 WHERE document_id = ?3",
                params![manifest_hash.as_str(), now, document_id],
            )?;
        }

        tx.commit()?;
        Ok(outcome)
    }


    /// Read a derived artefact from the manifest at `version_id`. Used
    /// to surface OCR transcripts in the UI without round-tripping
    /// through `reconstruct`.
    pub fn read_derived_artefact(
        &self,
        version_id: VersionId,
        path: &str,
    ) -> Result<Option<Vec<u8>>> {
        let entry = self.get_version(version_id)?;
        let manifest_bytes = self.blobs.read_to_vec(&entry.manifest_hash)?;
        let manifest = Manifest::from_canonical_json(&manifest_bytes)?;
        let Some(f) = manifest.files.iter().find(|f| f.derived && f.path == path) else {
            return Ok(None);
        };
        Ok(Some(self.blobs.read_to_vec(&f.sha256)?))
    }
}
