//! Library API for disk-side imports (`Library::import_file`) and the
//! private `finalize_import` helper that handles the deduplicated-blob
//! rollback case.

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

    /// Import a PDF or EPUB from disk into the library. Generates a fresh
    /// document UUID, builds the `.metadata` and `.content` JSON files the
    /// reMarkable expects, stores all three blobs (metadata, content, body),
    /// and records a first version with `Source::Imported`. The new document
    /// becomes outbound on the next `plan_push`.
    ///
    /// `body_kind` selects the file extension and `fileType` field used in
    /// `.content`. `visible_name` is what shows up on the tablet — typically
    /// the source filename without extension.
    pub fn import_file(
        &self,
        source_path: &Path,
        body_kind: ImportKind,
        visible_name: &str,
        parent_id: Option<&str>,
    ) -> Result<DocumentSummary> {
        use crate::manifest::ManifestFile;
        // record_version below already takes the write_lock; we don't
        // grab it here to avoid double-locking the same thread (std
        // Mutex is not reentrant). Calls to put_blob in between are
        // safe because the GC's live-set query and the import's record
        // are serialised through record_version's lock.

        // Stream the body file rather than `fs::read` it whole. A
        // 500 MB PDF would otherwise allocate 500 MB just to hand to
        // `put_blob` — the blob store already supports a streaming
        // path that keeps peak memory at one 64 KB buffer.
        let mut body_file = std::fs::File::open(source_path)?;
        let body_put = self.put_blob_from_reader(&mut body_file)?;
        let document_id = uuid::Uuid::new_v4().to_string();
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        // The tablet stores `lastModified` as a unix-millis string.
        let last_modified_ms =
            i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
                .unwrap_or(i64::MAX);

        let metadata_json = serde_json::json!({
            "visibleName": visible_name,
            "type": "DocumentType",
            "parent": parent_id.unwrap_or(""),
            "lastModified": last_modified_ms.to_string(),
            "lastOpened": "",
            "lastOpenedPage": 0,
            "version": 1,
            "pinned": false,
            "synced": false,
            "modified": false,
            "deleted": false,
            "metadatamodified": false,
        });
        let content_json = body_kind.content_json();

        let metadata_bytes = serde_json::to_vec_pretty(&metadata_json)?;
        let content_bytes = serde_json::to_vec_pretty(&content_json)?;

        let metadata_put = self.put_blob(&metadata_bytes)?;
        let content_put = self.put_blob(&content_bytes)?;
        // `body_put` was produced above before any allocation of the
        // file bytes — leaving the variable here keeps the manifest
        // construction below unchanged.

        let mut manifest = Manifest::new(&document_id, body_kind.doc_type(), visible_name);
        manifest.metadata = metadata_json;
        manifest.content_meta = content_json;
        manifest.files = vec![
            ManifestFile {
                path: format!("{document_id}.metadata"),
                sha256: metadata_put.hash.clone(),
                size: metadata_put.size,
                mode: 0o644,
                derived: false,
            },
            ManifestFile {
                path: format!("{document_id}.content"),
                sha256: content_put.hash.clone(),
                size: content_put.size,
                mode: 0o644,
                derived: false,
            },
            ManifestFile {
                path: format!("{document_id}.{}", body_kind.extension()),
                sha256: body_put.hash.clone(),
                size: body_put.size,
                mode: 0o644,
                derived: false,
            },
        ];

        self.finalize_import(
            manifest,
            body_put,
            metadata_put,
            content_put,
            visible_name,
            now,
        )
    }


    /// Record an imported document's manifest as a new version, or
    /// unlink the freshly-staged blobs if the recording fails.
    ///
    /// Issue #39: without the unlink, three blobs (body / metadata /
    /// content) sit unreferenced on disk for the 60 s GC grace window
    /// after `record_version` errors (db busy timeout after retries,
    /// the archived-vs-import gate from #31, a serialization error,
    /// …). If the user closes the app inside that window the dropped
    /// PDF/EPUB is silently lost — recoverable in principle by
    /// inspecting `blobs/` but the user has no clue where the bytes
    /// went. Proactively `fs::remove_file` each Stored blob in the
    /// Err branch so the post-state is binary: either the import is
    /// recorded, or no on-disk trace of it remains. Deduplicated
    /// puts are skipped — the bytes were already there before this
    /// call and some other manifest references them. Unlink is
    /// best-effort because GC's grace still reaps any blob we fail
    /// to remove; surfacing the unlink error would mask the
    /// underlying record_version failure the caller actually needs
    /// to see.
    pub(super) fn finalize_import(
        &self,
        manifest: Manifest,
        body_put: crate::blob::PutResult,
        metadata_put: crate::blob::PutResult,
        content_put: crate::blob::PutResult,
        visible_name: &str,
        now: String,
    ) -> Result<DocumentSummary> {
        let outcome = match self.record_version(&manifest, Source::Imported) {
            Ok(o) => o,
            Err(e) => {
                for put in [&body_put, &metadata_put, &content_put] {
                    if matches!(put.outcome, crate::blob::PutOutcome::Stored) {
                        let path = self.blobs.path_for(&put.hash);
                        let _ = std::fs::remove_file(&path);
                    }
                }
                return Err(e);
            }
        };

        Ok(DocumentSummary {
            document_id: manifest.document_id,
            visible_name: visible_name.to_string(),
            doc_type: manifest.doc_type,
            current_manifest: outcome.manifest_hash,
            current_version_id: outcome.version_id,
            last_observed_at: now,
            parent: None,
            size_bytes: metadata_put.size + content_put.size + body_put.size,
            page_count: None,
            has_unpushed_changes: true,
        })
    }
}
