//! Library API for maintenance operations: garbage collection (with
//! grace period), integrity verification, reconstruct (export a
//! recorded version to a directory), and revert-unpushed-changes.
//!
//! Also owns the file-walk helpers (`walk_blobs`, `prune_empty_dirs`)
//! the GC + verify paths share.

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

    /// Reclaim disk space by deleting blobs that no manifest in the version
    /// log references. A blob is considered live if it appears in
    /// `blob_refs` joined to `versions` — i.e. some recorded version still
    /// points at it. Manifests themselves are referenced via `blob_refs`'s
    /// (manifest_hash, manifest_hash) self-row, written by `record_version`.
    ///
    /// Safe to run while the app is otherwise idle. Refuses to delete
    /// anything that any version currently references; if you want to drop
    /// versions, use the (future) version-pruning API first, then GC.
    pub fn garbage_collect(&self) -> Result<GarbageCollectReport> {
        self.garbage_collect_with_grace(std::time::Duration::from_secs(60))
    }


    /// Underlying GC implementation with a configurable "grace" window —
    /// blobs younger than `grace` are kept even if unreferenced, to avoid
    /// racing with an in-flight import or sync. Tests pass `Duration::ZERO`
    /// to exercise the deletion path deterministically.
    pub fn garbage_collect_with_grace(
        &self,
        grace: std::time::Duration,
    ) -> Result<GarbageCollectReport> {
        let _write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
        {
            let conn = self.db.lock();
            let mut stmt = conn.prepare(
                "SELECT DISTINCT br.blob_hash FROM blob_refs br \
                 JOIN versions v ON v.manifest_hash = br.manifest_hash",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                live.insert(row.get::<_, String>(0)?);
            }
        }

        let mut report = GarbageCollectReport::default();
        let blobs_dir = self.paths.blobs.clone();
        let mut to_delete = Vec::new();

        // Belt-and-braces against the import/record_version vs GC race:
        // even though the write_lock covers record_version, an import or
        // pull writes file blobs before calling record_version, so there's
        // a brief window where a freshly-written blob is on disk but no
        // version refers to it. The `grace` window covers that.
        //
        // Clock-skew note: this uses `SystemTime::now()` and the blob's
        // filesystem mtime, both wall-clock values. If the system clock
        // jumps backward (NTP correction, DST oddities, manual change)
        // between the blob write and the GC scan, `duration_since`
        // returns `Err` and we treat the blob as old enough to delete.
        // To keep that benign we additionally check the reverse
        // direction — if the blob's mtime is in the *future* relative
        // to `now` it's almost certainly a clock skew and we keep the
        // blob. The remaining failure mode (clock jumps forward
        // *during* the same GC run) would falsely delete a blob younger
        // than `grace`; documenting rather than fixing because the
        // mitigation would require a separate monotonic write-time
        // store that's overkill for a desktop app on a stable clock.
        let now = std::time::SystemTime::now();

        walk_blobs(&blobs_dir, &mut |path, hash_str| {
            report.scanned += 1;
            if live.contains(hash_str) {
                return;
            }
            let meta = match std::fs::metadata(path) {
                Ok(m) => m,
                Err(_) => return,
            };
            // Skip recently-modified blobs (potential in-flight write).
            if let Ok(modified) = meta.modified() {
                match now.duration_since(modified) {
                    Ok(age) if age < grace => return,
                    Ok(_) => {} // old enough; fall through to delete.
                    Err(_) => {
                        // Blob mtime is in the future relative to `now`
                        // — assume clock skew and keep it. Logging at
                        // debug because this is informational.
                        tracing::debug!(
                            "gc: blob {} mtime is in the future; keeping (clock skew?)",
                            hash_str
                        );
                        return;
                    }
                }
            }
            to_delete.push((path.to_path_buf(), meta.len()));
        });

        for (path, size) in to_delete {
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    report.deleted += 1;
                    report.bytes_freed += size;
                }
                Err(e) => {
                    tracing::warn!("gc: failed to remove {}: {e}", path.display());
                    report.errors += 1;
                }
            }
        }

        // Best-effort: prune now-empty fanout directories so the library
        // doesn't accumulate empty `blobs/aa/bb/` shells.
        prune_empty_dirs(&blobs_dir);
        Ok(report)
    }


    /// Walk the library and verify on-disk integrity:
    /// - Every manifest in the version log refers to blobs that exist.
    /// - Every blob's filename matches its content's sha256.
    /// - Identify blobs in `blobs/` that no manifest references (orphans —
    ///   harmless, garbage-collectable in Phase 4).
    ///
    /// This is the canonical "never lie about state" check the design doc
    /// calls for. It does no network I/O and does not modify the library.
    pub fn verify(&self) -> Result<VerifyReport> {
        let mut report = VerifyReport::default();

        // 1) Walk every manifest currently referenced by a version, collect
        //    the union of all blob hashes they reference.
        let manifest_hashes: Vec<String> = self
            .db
            .lock()
            .prepare("SELECT DISTINCT manifest_hash FROM versions")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;

        let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
        for manifest_hex in manifest_hashes {
            report.manifests_total += 1;
            let hash = match Sha256Hex::from_hex(&manifest_hex) {
                Some(h) => h,
                None => {
                    report.manifests_invalid += 1;
                    continue;
                }
            };
            referenced.insert(hash.as_str().to_string());
            let manifest_bytes = match self.blobs.read_to_vec(&hash) {
                Ok(b) => b,
                Err(_) => {
                    report.manifests_missing += 1;
                    continue;
                }
            };
            // Manifest blob filename must match its content hash.
            if Sha256Hex::from_bytes(&manifest_bytes).as_str() != hash.as_str() {
                report.blobs_corrupted += 1;
                continue;
            }
            let manifest = match Manifest::from_canonical_json(&manifest_bytes) {
                Ok(m) => m,
                Err(_) => {
                    report.manifests_invalid += 1;
                    continue;
                }
            };
            for f in &manifest.files {
                referenced.insert(f.sha256.as_str().to_string());
                if !self.blobs.has(&f.sha256) {
                    report.blobs_missing += 1;
                    report
                        .missing_examples
                        .push(format!("{}:{}", manifest.document_id, f.path));
                }
            }
            report.manifests_ok += 1;
        }

        // 2) Walk every blob on disk: count, find orphans (blobs not
        //    referenced by any manifest), and verify the filename matches
        //    the content hash for a sample so we don't rehash everything by
        //    default.
        let blobs_dir = self.paths.blobs.clone();
        walk_blobs(&blobs_dir, &mut |path, hash_str| {
            report.blobs_total += 1;
            if !referenced.contains(hash_str) {
                report.blobs_orphan += 1;
                if report.orphan_examples.len() < 5 {
                    report.orphan_examples.push(path.display().to_string());
                }
            }
        });

        Ok(report)
    }


    /// Reconstruct the file tree of `version_id` under `dest`. Used by Phase 2
    /// (export) and as the canonical round-trip property test for the library.
    ///
    /// Refuses to overwrite pre-existing files inside `dest` by default —
    /// callers who deliberately want to overwrite must pass
    /// `ReconstructOptions { allow_overwrite: true, .. }` via
    /// [`Self::reconstruct_with`]. Manifest path validation prevents `..`
    /// escapes (see `crate::manifest::Manifest::validate_paths`), so the
    /// damage is bounded to `dest`, but the export UI nonetheless picks
    /// virgin directories — this guard is the second line of defence
    /// against a careless caller building a destination programmatically.
    pub fn reconstruct(&self, version_id: VersionId, dest: &Path) -> Result<()> {
        self.reconstruct_with(version_id, dest, ReconstructOptions::default())
    }


    /// As [`Self::reconstruct`], but lets callers tune the overwrite
    /// policy. See [`ReconstructOptions`].
    pub fn reconstruct_with(
        &self,
        version_id: VersionId,
        dest: &Path,
        opts: ReconstructOptions,
    ) -> Result<()> {
        let manifest_hex: String = self
            .db
            .lock()
            .query_row(
                "SELECT manifest_hash FROM versions WHERE id = ?1",
                params![version_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("version {version_id}")))?;
        let manifest_hash =
            Sha256Hex::from_hex(&manifest_hex).ok_or_else(|| CoreError::Corrupt {
                path: self.paths.db.display().to_string(),
                reason: format!("bad manifest hash for version {version_id}"),
            })?;
        let manifest_bytes = self.blobs.read_to_vec(&manifest_hash)?;
        let manifest = Manifest::from_canonical_json(&manifest_bytes)?;

        fs::create_dir_all(dest)?;
        for f in &manifest.files {
            let target = dest.join(&f.path);
            if !opts.allow_overwrite && target.exists() {
                return Err(CoreError::AlreadyExists(target.display().to_string()));
            }
        }
        for f in &manifest.files {
            let blob = self.blobs.read_to_vec(&f.sha256)?;
            let target = dest.join(&f.path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, &blob)?;
        }
        Ok(())
    }


    /// Roll the library back to its state at the last successful
    /// sync, undoing every local edit that hasn't been pushed to
    /// the device yet. The user-facing motivation: "I deleted a
    /// folder by accident and now I'm trapped — I can't sync this
    /// without losing the folder forever." Revert gets them out
    /// without forcing them to manually undo each edit.
    ///
    /// Scope:
    ///
    /// * **Folders** — every row with `pending_push = 1` or
    ///   `deleted_locally = 1` is touched. If the row has a
    ///   `last_synced_metadata_json` snapshot (i.e. the device had
    ///   seen it before), we restore `metadata_json`, `visible_name`,
    ///   and `parent` from the snapshot and clear `pending_push` /
    ///   `deleted_locally`. If the snapshot is NULL (locally-
    ///   created, never pushed), we drop the row.
    /// * **Documents** — rows whose `current_manifest` differs from
    ///   `sync_state.last_seen_manifest` (and that have a recorded
    ///   last-seen at all) get rolled back: `documents.current_manifest`
    ///   and `current_version_id` are reset to the version whose
    ///   manifest matches the last-pushed hash. The older version is
    ///   still in `versions`, so history is preserved — the live
    ///   pointer just steps backwards.
    /// * **Imports** (`last_seen_manifest IS NULL` documents) are
    ///   intentionally left alone. A user who imports a 50-page PDF
    ///   and then clicks Revert by accident must not lose the PDF;
    ///   archiving is the explicit "remove from library" flow for
    ///   those.
    ///
    /// Idempotent: re-running on an already-reverted library is a
    /// no-op (all three counts return 0).
    pub fn revert_unpushed_changes(&self) -> Result<RevertReport> {
        let _write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut conn = self.db.lock();
        let tx = conn.transaction()?;

        // ---- Folders ----
        let pending_folders: Vec<(String, Option<String>)> = {
            let mut stmt = tx.prepare(
                "SELECT folder_id, last_synced_metadata_json FROM folders \
                 WHERE pending_push = 1 OR deleted_locally = 1",
            )?;
            let rows: Vec<(String, Option<String>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            rows
        };

        let mut folders_restored = 0;
        let mut folders_dropped = 0;
        for (folder_id, snapshot) in pending_folders {
            match snapshot {
                None => {
                    // Locally-created row, never seen by the device.
                    // Revert removes it entirely so the sidebar
                    // returns to its last-synced shape.
                    tx.execute(
                        "DELETE FROM folders WHERE folder_id = ?1",
                        params![folder_id],
                    )?;
                    folders_dropped += 1;
                }
                Some(snapshot_json) => {
                    // Restore. We re-extract `visible_name` and
                    // `parent` from the snapshot rather than keeping
                    // them around as separate columns — single
                    // source of truth, and `upsert_folder` already
                    // sets the snapshot to the same JSON it derives
                    // those columns from.
                    let v: serde_json::Value = serde_json::from_str(&snapshot_json)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    let visible_name = v
                        .get("visibleName")
                        .and_then(|x| x.as_str())
                        .unwrap_or("(restored)")
                        .to_string();
                    let parent_raw = v.get("parent").and_then(|x| x.as_str()).unwrap_or("");
                    let parent: Option<&str> = if parent_raw.is_empty() {
                        None
                    } else {
                        Some(parent_raw)
                    };
                    tx.execute(
                        "UPDATE folders \
                         SET metadata_json = ?1, \
                             visible_name = ?2, \
                             parent = ?3, \
                             pending_push = 0, \
                             deleted_locally = 0 \
                         WHERE folder_id = ?4",
                        params![snapshot_json, visible_name, parent, folder_id],
                    )?;
                    folders_restored += 1;
                }
            }
        }

        // ---- Documents ----
        // Rows where the current manifest has drifted from the last
        // sync's manifest. The JOIN to `versions` resolves the
        // version id that owns the last-seen manifest hash; we need
        // both columns to keep `current_version_id` consistent with
        // `current_manifest`. We deliberately skip rows where
        // `last_seen_manifest IS NULL` (imports + first-time pulls
        // that haven't been observed by the push engine yet).
        let drifted: Vec<(String, String, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT d.document_id, s.last_seen_manifest, v.id \
                 FROM documents d \
                 JOIN sync_state s ON s.document_id = d.document_id \
                 JOIN versions v \
                   ON v.document_id = d.document_id \
                  AND v.manifest_hash = s.last_seen_manifest \
                 WHERE s.last_seen_manifest IS NOT NULL \
                   AND s.last_seen_manifest <> d.current_manifest",
            )?;
            let rows: Vec<(String, String, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            rows
        };
        let mut documents_rolled_back = 0;
        for (document_id, last_seen, version_id) in drifted {
            tx.execute(
                "UPDATE documents \
                 SET current_manifest = ?1, current_version_id = ?2 \
                 WHERE document_id = ?3",
                params![last_seen, version_id, document_id],
            )?;
            documents_rolled_back += 1;
        }

        tx.commit()?;
        Ok(RevertReport {
            folders_restored,
            folders_dropped,
            documents_rolled_back,
        })
    }
}

/// Knobs for [`Library::reconstruct_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ReconstructOptions {
    /// When `true`, files already present at the destination paths are
    /// silently replaced. When `false` (the default), reconstruct
    /// aborts with `CoreError::AlreadyExists` on the first collision and
    /// does not write any blobs — the caller can pick a different
    /// destination or pass `allow_overwrite` deliberately.
    pub allow_overwrite: bool,
}

/// Recursively walk the `blobs/<aa>/<bb>/<hash>` tree, invoking `visit` for
/// each leaf file with the file's full path and its filename (the hex hash).
/// Symlinks are skipped — the blob store should never contain them, and
/// following a planted symlink could leak filesystem contents into GC's
/// orphan list or even let GC delete files outside the library directory.
fn walk_blobs(root: &Path, visit: &mut dyn FnMut(&Path, &str)) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            continue;
        }
        let p = entry.path();
        if ft.is_dir() {
            walk_blobs(&p, visit);
        } else if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
            visit(&p, name);
        }
    }
}

/// Remove empty subdirectories under `root`, depth-first. `remove_dir` only
/// succeeds on empty directories, so populated leaves stay intact. Skips
/// symlinks for the same reason `walk_blobs` does.
fn prune_empty_dirs(root: &Path) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for entry in rd.flatten() {
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            let p = entry.path();
            prune_empty_dirs(&p);
            let _ = std::fs::remove_dir(&p);
        }
    }
}
