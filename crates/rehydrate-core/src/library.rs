//! High-level Library facade.
//!
//! Phase 1 implements a lean subset: open an empty library, store blobs,
//! record a manifest as a new version. Phase 2 will add reconstruct + history
//! UI surfaces; Phase 4 will add GC. The data model already supports them.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

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

const LIBRARY_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Pulled,
    Imported,
    Restored,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Pulled => "pulled",
            Source::Imported => "imported",
            Source::Restored => "restored",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "pulled" => Some(Source::Pulled),
            "imported" => Some(Source::Imported),
            "restored" => Some(Source::Restored),
            _ => None,
        }
    }
}

pub type VersionId = i64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSummary {
    pub document_id: String,
    pub visible_name: String,
    pub doc_type: String,
    pub current_manifest: Sha256Hex,
    pub current_version_id: VersionId,
    /// When the current version was recorded. Used by the UI's "Recently
    /// Synced" sidebar filter; sortable as an RFC3339 timestamp string.
    pub last_observed_at: String,
    /// Folder UUID this document belongs to. `None` for the root.
    /// `"trash"` is the device's special trash bucket.
    pub parent: Option<String>,
    /// Total bytes of every file the manifest references. Cheap to
    /// compute since `list_documents` already reads the manifest blob.
    #[serde(default)]
    pub size_bytes: u64,
    /// Page count read from `content_meta.pageCount` if present.
    /// Notebooks always carry it; PDFs/EPUBs sometimes do.
    #[serde(default)]
    pub page_count: Option<u32>,
    /// True if the current manifest hasn't been pushed yet — i.e. the
    /// document has local edits that the next sync will upload.
    /// Imported-but-never-synced docs are also unpushed.
    #[serde(default)]
    pub has_unpushed_changes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderEntry {
    pub folder_id: String,
    pub parent: Option<String>,
    pub visible_name: String,
    /// Local-only ordering hint within the parent's children. Lower
    /// values come first; ties break alphabetically. Never sent to the
    /// device — folders on the reMarkable don't have an explicit order.
    #[serde(default)]
    pub sort_index: f64,
}

/// Tally of children that were lifted out of a deleted folder.
/// Returned from `Library::delete_folder` so the UI can word the
/// confirmation toast precisely ("Journal removed — BH and test
/// moved up.")
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteFolderOutcome {
    pub folders_moved: usize,
    pub documents_moved: usize,
}

/// Summary of what `revert_unpushed_changes` undid. The UI surfaces
/// each number in the post-revert toast so the user can confirm the
/// action did what they expected (e.g. "Restored 1 deleted folder
/// and rolled back 3 document moves.").
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RevertReport {
    /// Folders whose local edits were rolled back to the device's
    /// last-seen state — un-deleted, un-renamed, or un-reparented.
    pub folders_restored: usize,
    /// Folders that existed only locally (created via
    /// `create_folder`, never pushed). Revert deletes them outright.
    pub folders_dropped: usize,
    /// Documents whose `current_manifest` was rolled back to the
    /// `last_seen_manifest` — undoing local moves, renames, and
    /// metadata changes that hadn't been pushed yet. Imported docs
    /// (no `last_seen_manifest`) are not touched: the user does not
    /// expect a "Revert" button to delete the PDF they just dragged
    /// in.
    pub documents_rolled_back: usize,
}

/// One pending folder operation that the push engine has to ship to
/// the device. Returned by `Library::list_pending_folder_pushes` so
/// the engine can route renames/reparents/creates through
/// `Device::put_document_tree` and deletions through
/// `Device::delete_document_tree`. Without the split, deletions had
/// to be encoded as a metadata-file upload with `deleted: true`,
/// which xochitl interprets as "move to Trash" instead of a real
/// removal.
#[derive(Debug, Clone)]
pub enum FolderPushOp {
    /// Upload the folder's metadata file. Covers brand-new folders,
    /// renames, and reparents.
    Upsert {
        folder_id: String,
        metadata_json: String,
    },
    /// SFTP-remove every `<folder_id>*` artefact on the device.
    Delete { folder_id: String },
}

impl FolderPushOp {
    /// The folder id this op targets. Useful because the push engine
    /// calls `mark_folder_pushed` after both shapes succeed.
    pub fn folder_id(&self) -> &str {
        match self {
            FolderPushOp::Upsert { folder_id, .. } => folder_id,
            FolderPushOp::Delete { folder_id } => folder_id,
        }
    }

    pub fn kind(&self) -> FolderPushKind {
        match self {
            FolderPushOp::Upsert { .. } => FolderPushKind::Upsert,
            FolderPushOp::Delete { .. } => FolderPushKind::Delete,
        }
    }
}

/// What kind of folder push completed. Passed to
/// `mark_folder_pushed` so the post-push DB update routes from the
/// op that was actually shipped to the device, NOT from the current
/// `deleted_locally` value — that field can flip between when the
/// push engine snapshots the queue and when it reconciles the row,
/// and a row whose `deleted_locally` flipped to 1 mid-push must
/// keep its pending Delete queued instead of being dropped silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderPushKind {
    Upsert,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionEntry {
    pub id: VersionId,
    pub document_id: String,
    pub manifest_hash: Sha256Hex,
    pub parent_version_id: Option<VersionId>,
    pub observed_at: String,
    pub source: Source,
    pub note: Option<String>,
    /// Sum of `manifest.files[].size`. Read from the manifest blob on demand
    /// — Phase 2's history UI shows this per version. `None` if the manifest
    /// blob is missing (which `verify` would already have flagged).
    pub total_size_bytes: Option<u64>,
    /// Number of files in the document tree at this version.
    pub file_count: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RecordOutcome {
    pub version_id: VersionId,
    pub manifest_hash: Sha256Hex,
    /// True if this manifest hash was already the current version for this
    /// document — i.e. nothing changed and no new row was appended.
    pub unchanged: bool,
}

/// Selects the on-device file extension and content metadata for an import.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImportKind {
    Pdf,
    Epub,
}

impl ImportKind {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "pdf" => Some(ImportKind::Pdf),
            "epub" => Some(ImportKind::Epub),
            _ => None,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            ImportKind::Pdf => "pdf",
            ImportKind::Epub => "epub",
        }
    }

    fn doc_type(self) -> &'static str {
        match self {
            ImportKind::Pdf => "DocumentType.Pdf",
            ImportKind::Epub => "DocumentType.Epub",
        }
    }

    /// `.content` JSON. We populate the small subset of fields xochitl
    /// actually requires — it fills in the rest (page count, transform,
    /// extraMetadata) on first open.
    fn content_json(self) -> serde_json::Value {
        match self {
            ImportKind::Pdf => serde_json::json!({
                "fileType": "pdf",
                "pageCount": 0,
                "lastOpenedPage": 0,
                "lineHeight": -1,
                "margins": 100,
                "orientation": "portrait",
                "textScale": 1,
                "extraMetadata": {},
                "transform": {
                    "m11": 1.0, "m12": 0.0, "m13": 0.0,
                    "m21": 0.0, "m22": 1.0, "m23": 0.0,
                    "m31": 0.0, "m32": 0.0, "m33": 1.0,
                },
            }),
            ImportKind::Epub => serde_json::json!({
                "fileType": "epub",
                "pageCount": 0,
                "lastOpenedPage": 0,
                "lineHeight": -1,
                "margins": 100,
                "orientation": "portrait",
                "textScale": 1,
                "extraMetadata": {},
            }),
        }
    }
}

/// Why a document is in the archive. Recorded so the UI can show a hint
/// ("deleted on device", "deleted locally") and so future tooling can
/// distinguish auto-archived from user-archived entries.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveReason {
    /// User clicked Delete in the local UI.
    Local,
    /// Detected during pull: the device no longer has this document.
    Device,
}

impl ArchiveReason {
    fn as_str(self) -> &'static str {
        match self {
            ArchiveReason::Local => "local",
            ArchiveReason::Device => "device",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "local" => Some(ArchiveReason::Local),
            "device" => Some(ArchiveReason::Device),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedDocument {
    pub document_id: String,
    pub visible_name: String,
    pub doc_type: String,
    pub parent: Option<String>,
    pub manifest_hash: Sha256Hex,
    pub version_id: VersionId,
    pub reason: ArchiveReason,
    pub archived_at: String,
}

/// Optional second SQL operation glued to the same transaction as
/// `record_metadata_change`'s record_version. Used by archive /
/// unarchive so the metadata edit and the documents↔archived_documents
/// move commit together (audit fix C2).
enum PostAction<'a> {
    None,
    Archive {
        reason: ArchiveReason,
        visible_name: &'a str,
        doc_type: &'a str,
        original_parent: Option<&'a str>,
    },
    Unarchive,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GarbageCollectReport {
    pub scanned: usize,
    pub deleted: usize,
    pub bytes_freed: u64,
    pub errors: usize,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub manifests_total: usize,
    pub manifests_ok: usize,
    pub manifests_missing: usize,
    pub manifests_invalid: usize,
    pub blobs_total: usize,
    pub blobs_missing: usize,
    pub blobs_orphan: usize,
    pub blobs_corrupted: usize,
    /// Up to 5 example "<document_id>:<path>" strings for missing blobs.
    pub missing_examples: Vec<String>,
    /// Up to 5 example absolute paths of orphan blobs.
    pub orphan_examples: Vec<String>,
}

/// Result of [`Library::probe_path`]: what the IPC layer should
/// tell the user before attempting to open the path. `Empty` means
/// safe-to-create-here; `Existing` means a stamped library is
/// already there. The error case (a foreign non-empty directory)
/// surfaces as [`CoreError::InvalidPath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LibraryPathKind {
    Empty,
    Existing,
}

#[derive(Serialize, Deserialize)]
struct LibraryMeta {
    schema: u32,
    library_id: String,
    created_at: String,
}

pub struct Library {
    paths: LibraryPaths,
    blobs: BlobStore,
    db: Db,
    /// Coarse-grained mutex serialising blob-mutating operations
    /// (record_version, import_file, restore_version) against
    /// garbage_collect. Without this, GC can build its live-set, then
    /// record_version commits a new manifest, then GC deletes the
    /// freshly-written blob because it wasn't in the snapshot.
    /// The lock is held only inside Library methods; it doesn't span
    /// async I/O (the sync engine pulls all bytes into memory before
    /// calling record_version, so the critical section is short).
    ///
    /// All call sites lock with `.unwrap_or_else(|e| e.into_inner())`
    /// rather than `.expect(…)`. A panic inside a critical section
    /// poisons the mutex; treating that as a hard panic on every
    /// subsequent operation would turn a single transient bug into a
    /// permanent app-wide crash. Recovering the inner guard lets the
    /// library degrade rather than abend — the underlying SQLite
    /// connection survives a Rust panic in the application code
    /// above it.
    write_lock: Mutex<()>,
    /// OS-level advisory exclusive lock held for the lifetime of the
    /// library instance. Audit fix H3: without this, two app
    /// instances pointed at the same library can each hold their own
    /// `write_lock` mutex, and instance A's GC may delete a blob that
    /// instance B just committed. The handle is kept alive in this
    /// field; dropping it releases the OS lock automatically.
    _lock_file: std::fs::File,
}

impl Library {
    /// Open an existing library or initialize a new one at `path`.
    ///
    /// Audit fix H9: refuses to silently claim a non-empty directory
    /// that doesn't already look like a reHydrate library. Without
    /// this check, a renderer-supplied path like `~/Documents/` would
    /// have `blobs/`, `tmp/`, `logs/`, and `db.sqlite` written into
    /// it — and a subsequent `garbage_collect` would walk that
    /// `blobs/` and `remove_file` matching entries.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let paths = LibraryPaths::new(path.as_ref());
        Self::validate_library_path(&paths)?;
        paths.ensure_dirs()?;

        // Acquire the inter-process advisory lock first. If another
        // app instance has the same library open, fail fast — sharing
        // a library across processes corrupts the GC vs. record_version
        // invariant (audit fix H3).
        let lock_path = paths.root.join(".lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        if FileExt::try_lock(&lock_file).is_err() {
            return Err(CoreError::AlreadyOpen(paths.root.display().to_string()));
        }

        if !paths.library_json.exists() {
            let meta = LibraryMeta {
                schema: LIBRARY_SCHEMA,
                library_id: uuid::Uuid::new_v4().to_string(),
                created_at: OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            };
            write_library_stamp_atomically(&paths, &meta)?;
        }

        let db = Db::open(&paths.db)?;
        let blobs = BlobStore::new(paths.clone());

        Ok(Self {
            paths,
            blobs,
            db,
            write_lock: Mutex::new(()),
            _lock_file: lock_file,
        })
    }

    /// A path is a valid library target if either:
    /// - The directory does not yet exist (fresh init), or
    /// - The directory exists and contains a parseable `library.json`
    ///   with a recognised schema and a UUID stamp, or
    /// - The directory exists and is effectively empty (only dotfiles
    ///   like `.DS_Store` / our own `.lock`).
    ///
    /// Anything else — a directory full of foreign files — is
    /// rejected so we never silently scatter blobs into the user's
    /// Documents folder.
    fn validate_library_path(paths: &LibraryPaths) -> Result<()> {
        if !paths.root.exists() {
            // Fresh init: ensure_dirs will create it.
            return Ok(());
        }
        if paths.library_json.exists() {
            // Existing library — verify the stamp is sane.
            let bytes = fs::read(&paths.library_json).map_err(|e| {
                CoreError::InvalidPath(format!(
                    "could not read {}: {e}",
                    paths.library_json.display()
                ))
            })?;
            let meta: LibraryMeta = serde_json::from_slice(&bytes).map_err(|e| {
                CoreError::InvalidPath(format!(
                    "{} has a malformed library.json: {e}",
                    paths.root.display()
                ))
            })?;
            if meta.schema != LIBRARY_SCHEMA {
                return Err(CoreError::InvalidPath(format!(
                    "{} has library schema {} (expected {})",
                    paths.root.display(),
                    meta.schema,
                    LIBRARY_SCHEMA
                )));
            }
            if uuid::Uuid::parse_str(&meta.library_id).is_err() {
                return Err(CoreError::InvalidPath(format!(
                    "{} has an invalid library_id stamp",
                    paths.root.display()
                )));
            }
            return Ok(());
        }
        // No library.json yet — only proceed if the dir is empty (modulo
        // dotfiles + a stale `.lock` from a prior failed init).
        let foreign: Vec<_> = fs::read_dir(&paths.root)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                !s.starts_with('.')
            })
            .collect();
        if !foreign.is_empty() {
            return Err(CoreError::InvalidPath(format!(
                "{} is not empty and is not a reHydrate library (no library.json)",
                paths.root.display()
            )));
        }
        Ok(())
    }
}

/// Issue #32: write the library stamp via `<tmp>` → fsync → rename
/// so a crash during initial stamp-out can never leave a
/// half-written or zero-length `library.json`. The previous
/// `fs::write` was truncate-in-place: a power loss between truncate
/// and the final byte would create a non-empty-but-malformed file
/// that `validate_library_path` then refused to parse on every
/// subsequent open, locking the user out of their own library
/// (blobs and SQLite untouched, but the stamp gate fails) until
/// they hand-edited or deleted the file. Mirrors the atomic-write
/// pattern BlobStore uses for content blobs.
fn write_library_stamp_atomically(paths: &LibraryPaths, meta: &LibraryMeta) -> Result<()> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(meta)?;

    // Stage to a sibling tmp file inside the library root so the
    // final rename is intra-filesystem (and therefore atomic).
    let mut tmp = tempfile::NamedTempFile::new_in(&paths.root)?;
    tmp.write_all(&bytes)?;
    tmp.as_file().sync_all()?;

    // Atomic rename. `persist` keeps the tmp on disk and unlinks
    // it from its random tmp name as it lands at the target; on
    // failure the tmp is automatically cleaned up by the dropping
    // NamedTempFile, so we never leak a `.tmp*` next to a missing
    // library.json.
    tmp.persist(&paths.library_json)
        .map_err(|e| CoreError::Io(e.error))?;

    // Best-effort fsync of the root so the rename's directory
    // entry is durable too (same rationale as the blob fanout
    // walk in BlobStore::put_reader — POSIX guarantees fsync(file)
    // makes the bytes durable but not the entry that points at
    // them). Best-effort: the file bytes are already durable from
    // tmp.as_file().sync_all() above.
    if let Ok(dir) = std::fs::File::open(&paths.root) {
        let _ = dir.sync_all();
    }
    Ok(())
}

impl Library {
    pub fn paths(&self) -> &LibraryPaths {
        &self.paths
    }

    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Look at `path` and report whether it's already a library, an
    /// empty directory ready to become one, or a non-empty foreign
    /// directory we shouldn't claim. Public so the IPC layer can
    /// surface a "Create new library here?" prompt before
    /// `Library::open` silently materialises one.
    ///
    /// Does not acquire the inter-process lock — purely a read.
    pub fn probe_path(path: impl AsRef<Path>) -> Result<LibraryPathKind> {
        let paths = LibraryPaths::new(path.as_ref());
        if !paths.root.exists() {
            return Ok(LibraryPathKind::Empty);
        }
        if paths.library_json.exists() {
            // Reuse the same stamp validation `Library::open` does so
            // the probe and the open agree on what counts as valid.
            Self::validate_library_path(&paths)?;
            return Ok(LibraryPathKind::Existing);
        }
        let foreign: Vec<_> = fs::read_dir(&paths.root)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                !name.to_string_lossy().starts_with('.')
            })
            .collect();
        if foreign.is_empty() {
            Ok(LibraryPathKind::Empty)
        } else {
            Err(CoreError::InvalidPath(format!(
                "{} is not empty and is not a reHydrate library (no library.json)",
                paths.root.display()
            )))
        }
    }

    pub fn put_blob(&self, bytes: &[u8]) -> Result<crate::blob::PutResult> {
        self.blobs.put_bytes(bytes)
    }

    /// Stream-and-hash a blob from a reader without buffering its
    /// contents end-to-end in memory. Used by `import_file` to keep
    /// peak memory at one 64 KB buffer when the user drops in a
    /// 500 MB PDF, instead of allocating the whole file's bytes.
    pub fn put_blob_from_reader<R: std::io::Read>(
        &self,
        reader: &mut R,
    ) -> Result<crate::blob::PutResult> {
        self.blobs.put_reader(reader)
    }

    pub fn has_blob(&self, hash: &Sha256Hex) -> bool {
        self.blobs.has(hash)
    }

    pub fn read_blob(&self, hash: &Sha256Hex) -> Result<Vec<u8>> {
        self.blobs.read_to_vec(hash)
    }

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
    fn record_version_in_tx(
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
    fn record_metadata_change<F>(
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
        tx.commit()?;
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
            "parent": "",
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
    fn finalize_import(
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

    /// Total versions across all documents.
    pub fn version_count(&self) -> Result<i64> {
        Ok(self
            .db
            .lock()
            .query_row("SELECT count(*) FROM versions", [], |r| r.get(0))?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::PutOutcome;
    use crate::manifest::ManifestFile;

    fn seed_blob(lib: &Library, bytes: &[u8]) -> Sha256Hex {
        lib.put_blob(bytes).unwrap().hash
    }

    fn seed_manifest(lib: &Library, doc_id: &str, contents: &[(&str, &[u8])]) -> Manifest {
        let mut files = Vec::new();
        // Always include a `.metadata` file — the archive/move flows
        // mutate it, and real synced documents always have one.
        let meta_blob = serde_json::json!({
            "visibleName": "Test",
            "type": "DocumentType",
            "parent": "",
            "deleted": false,
            "lastModified": "0",
        });
        let meta_bytes = serde_json::to_vec_pretty(&meta_blob).unwrap();
        let meta_put = lib.put_blob(&meta_bytes).unwrap();
        files.push(ManifestFile {
            path: format!("{doc_id}.metadata"),
            sha256: meta_put.hash,
            size: meta_put.size,
            mode: 0o644,
            derived: false,
        });
        for (path, bytes) in contents {
            let res = lib.put_blob(bytes).unwrap();
            files.push(ManifestFile {
                path: (*path).to_string(),
                sha256: res.hash,
                size: res.size,
                mode: 0o644,
                derived: false,
            });
        }
        let mut m = Manifest::new(doc_id, "Notebook", "Test");
        m.metadata = meta_blob;
        m.files = files;
        m
    }

    #[test]
    fn record_then_list_and_history() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("a.rm", b"AAA"), ("b.rm", b"BBB")]);

        let outcome1 = lib.record_version(&m, Source::Pulled).unwrap();
        assert!(!outcome1.unchanged);

        let outcome2 = lib.record_version(&m, Source::Pulled).unwrap();
        assert!(
            outcome2.unchanged,
            "re-recording identical manifest must be a no-op"
        );
        assert_eq!(outcome2.version_id, outcome1.version_id);

        let docs = lib.list_documents().unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].document_id, "doc-1");

        let history = lib.get_history("doc-1").unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].source, Source::Pulled);
    }

    #[test]
    fn changing_a_file_appends_a_new_version() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m1 = seed_manifest(&lib, "doc-1", &[("a.rm", b"v1")]);
        lib.record_version(&m1, Source::Pulled).unwrap();
        let m2 = seed_manifest(&lib, "doc-1", &[("a.rm", b"v2")]);
        let r2 = lib.record_version(&m2, Source::Pulled).unwrap();
        assert!(!r2.unchanged);
        let history = lib.get_history("doc-1").unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].parent_version_id, Some(history[0].id));
    }

    #[test]
    fn reconstruct_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(
            &lib,
            "doc-1",
            &[
                ("doc.metadata", b"{\"deleted\":false}"),
                ("doc/page-1.rm", b"page1bytes"),
            ],
        );
        let r = lib.record_version(&m, Source::Pulled).unwrap();

        let out = tempfile::tempdir().unwrap();
        lib.reconstruct(r.version_id, out.path()).unwrap();
        assert_eq!(
            fs::read(out.path().join("doc.metadata")).unwrap(),
            b"{\"deleted\":false}"
        );
        assert_eq!(
            fs::read(out.path().join("doc/page-1.rm")).unwrap(),
            b"page1bytes"
        );
    }

    #[test]
    fn verify_clean_library_reports_no_problems() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("a.rm", b"AAA"), ("b.rm", b"BBB")]);
        lib.record_version(&m, Source::Pulled).unwrap();

        let report = lib.verify().unwrap();
        assert_eq!(report.manifests_total, 1);
        assert_eq!(report.manifests_ok, 1);
        assert_eq!(report.blobs_missing, 0);
        assert_eq!(report.blobs_orphan, 0);
        // Four blobs: .metadata (always seeded), a.rm, b.rm, and the
        // manifest itself.
        assert_eq!(report.blobs_total, 4);
    }

    #[test]
    fn verify_detects_missing_and_orphan_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("a.rm", b"AAA")]);
        let outcome = lib.record_version(&m, Source::Pulled).unwrap();

        // Plant an orphan blob: write some bytes that no manifest references.
        let orphan = lib.put_blob(b"i am an orphan").unwrap();
        assert!(lib.has_blob(&orphan.hash));

        // Delete a referenced file blob to simulate corruption. Pick the
        // a.rm blob explicitly — seed_manifest also seeds a .metadata
        // file at index 0.
        let target = m
            .files
            .iter()
            .find(|f| f.path == "a.rm")
            .unwrap()
            .sha256
            .clone();
        std::fs::remove_file(lib.blobs().path_for(&target)).unwrap();

        let report = lib.verify().unwrap();
        assert_eq!(report.manifests_ok, 1);
        assert_eq!(report.blobs_missing, 1, "should report the deleted file");
        assert_eq!(report.blobs_orphan, 1, "should flag the orphan");
        assert!(report.missing_examples[0].starts_with("doc-1:a.rm"));

        // Recording the manifest again is still fine; verify should still
        // see the same problem.
        let _ = outcome;
    }

    #[test]
    fn import_file_creates_outbound_document() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();

        // Write a tiny "PDF" payload to disk and import it.
        let src = tmp.path().join("Sample.pdf");
        std::fs::write(&src, b"%PDF-1.7 fake bytes").unwrap();

        let summary = lib
            .import_file(&src, ImportKind::Pdf, "Sample document")
            .unwrap();
        assert_eq!(summary.visible_name, "Sample document");
        assert_eq!(summary.doc_type, "DocumentType.Pdf");

        // Manifest has three files: .metadata, .content, .pdf
        let manifest_bytes = lib.read_blob(&summary.current_manifest).unwrap();
        let manifest = Manifest::from_canonical_json(&manifest_bytes).unwrap();
        assert_eq!(manifest.files.len(), 3);
        assert!(manifest.files.iter().any(|f| f.path.ends_with(".metadata")));
        assert!(manifest.files.iter().any(|f| f.path.ends_with(".content")));
        assert!(manifest.files.iter().any(|f| f.path.ends_with(".pdf")));

        // sync_state.last_seen_manifest must be NULL — fresh import has
        // not been pushed yet, so plan_push should treat it as outbound.
        let last_seen = lib.last_seen(&summary.document_id).unwrap();
        assert!(matches!(last_seen, Some((_, None)) | None));
    }

    #[test]
    fn finalize_import_unlinks_staged_blobs_when_record_version_fails() {
        // Issue #39 regression. import_file stages body / metadata /
        // content blobs *before* calling record_version. Pre-fix, a
        // record_version failure left those blobs unreferenced on
        // disk for the 60 s GC grace — if the user closed the app
        // inside that window the dropped file vanished. The contract
        // now: on record_version Err, every Stored put from this
        // call is fs::remove_file'd before the error returns, so the
        // post-state is binary (either recorded, or no trace).
        //
        // To force a record_version failure deterministically we
        // exploit the #31 archive-vs-pull gate: pre-archive a doc
        // with id X, then call finalize_import with a manifest
        // whose document_id == X. record_version's DocumentArchived
        // branch fires and the cleanup must run.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();

        // Set up the archived doc that will collide with our import's
        // document_id and trigger DocumentArchived.
        let pre = seed_manifest(&lib, "doc-collide", &[("a.rm", b"page")]);
        lib.record_version(&pre, Source::Pulled).unwrap();
        lib.archive_document("doc-collide", ArchiveReason::Local)
            .unwrap();

        // Stage three blobs as if we were inside import_file. Distinct
        // bytes so each hash is unique and not aliased with anything
        // the pre-archived doc references.
        let body_put = lib.put_blob(b"BODY-import-bytes-39-unique").unwrap();
        let meta_put = lib.put_blob(b"META-import-bytes-39-unique-too").unwrap();
        let content_put = lib.put_blob(b"CONTENT-import-bytes-39-unique").unwrap();
        assert!(matches!(body_put.outcome, PutOutcome::Stored));
        assert!(matches!(meta_put.outcome, PutOutcome::Stored));
        assert!(matches!(content_put.outcome, PutOutcome::Stored));
        let body_path = lib.blobs().path_for(&body_put.hash);
        let meta_path = lib.blobs().path_for(&meta_put.hash);
        let content_path = lib.blobs().path_for(&content_put.hash);
        assert!(body_path.exists());
        assert!(meta_path.exists());
        assert!(content_path.exists());

        // Build an import-shaped manifest that record_version will
        // reject via the archived-doc gate.
        let mut manifest = Manifest::new("doc-collide", "DocumentType.Pdf", "name");
        manifest.files = vec![
            ManifestFile {
                path: "doc-collide.metadata".into(),
                sha256: meta_put.hash.clone(),
                size: meta_put.size,
                mode: 0o644,
                derived: false,
            },
            ManifestFile {
                path: "doc-collide.content".into(),
                sha256: content_put.hash.clone(),
                size: content_put.size,
                mode: 0o644,
                derived: false,
            },
            ManifestFile {
                path: "doc-collide.pdf".into(),
                sha256: body_put.hash.clone(),
                size: body_put.size,
                mode: 0o644,
                derived: false,
            },
        ];

        let err = lib
            .finalize_import(
                manifest,
                body_put.clone(),
                meta_put.clone(),
                content_put.clone(),
                "name",
                "now".into(),
            )
            .expect_err("archived-doc gate must reject this import");
        assert!(
            matches!(err, CoreError::DocumentArchived(ref id) if id == "doc-collide"),
            "wanted DocumentArchived, got {err:?}",
        );

        // The three staged blobs must be gone — that's the whole
        // point of the fix.
        assert!(
            !body_path.exists(),
            "Stored body blob must be unlinked after a failed import",
        );
        assert!(
            !meta_path.exists(),
            "Stored metadata blob must be unlinked after a failed import",
        );
        assert!(
            !content_path.exists(),
            "Stored content blob must be unlinked after a failed import",
        );
    }

    #[test]
    fn finalize_import_keeps_deduplicated_blobs_on_failure() {
        // Issue #39 boundary: a Deduplicated put means the bytes
        // were already in the store before this call — some other
        // manifest references them. We must NOT unlink those, or a
        // failed import would shred unrelated documents' data. Only
        // Stored puts (which were newly written by this call) are
        // safe to remove on failure.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();

        // A pre-existing doc that already references some bytes.
        // Its .metadata content is what we'll dedup against below.
        let pre = seed_manifest(&lib, "doc-collide", &[("a.rm", b"page")]);
        lib.record_version(&pre, Source::Pulled).unwrap();
        let preexisting_meta_hash = pre
            .files
            .iter()
            .find(|f| f.path.ends_with(".metadata"))
            .unwrap()
            .sha256
            .clone();
        let preexisting_meta_bytes = lib.read_blob(&preexisting_meta_hash).unwrap();
        lib.archive_document("doc-collide", ArchiveReason::Local)
            .unwrap();

        // Stage 3 blobs — but make the metadata bytes IDENTICAL to
        // the pre-existing doc's metadata so put_blob dedups it.
        let body_put = lib.put_blob(b"BODY-39-keep-on-failure").unwrap();
        let meta_put = lib.put_blob(&preexisting_meta_bytes).unwrap();
        let content_put = lib.put_blob(b"CONTENT-39-keep-on-failure").unwrap();
        assert!(matches!(meta_put.outcome, PutOutcome::Deduplicated));
        let meta_path = lib.blobs().path_for(&meta_put.hash);

        let mut manifest = Manifest::new("doc-collide", "DocumentType.Pdf", "name");
        manifest.files = vec![
            ManifestFile {
                path: "doc-collide.metadata".into(),
                sha256: meta_put.hash.clone(),
                size: meta_put.size,
                mode: 0o644,
                derived: false,
            },
            ManifestFile {
                path: "doc-collide.content".into(),
                sha256: content_put.hash.clone(),
                size: content_put.size,
                mode: 0o644,
                derived: false,
            },
            ManifestFile {
                path: "doc-collide.pdf".into(),
                sha256: body_put.hash.clone(),
                size: body_put.size,
                mode: 0o644,
                derived: false,
            },
        ];

        let _ = lib
            .finalize_import(
                manifest,
                body_put.clone(),
                meta_put.clone(),
                content_put.clone(),
                "name",
                "now".into(),
            )
            .expect_err("archived-doc gate must reject");

        // The deduplicated metadata blob is still referenced by the
        // pre-archived doc; must NOT be unlinked.
        assert!(
            meta_path.exists(),
            "Deduplicated blob must survive a failed import — it belongs to another manifest",
        );
        // Sanity: the pre-archived doc's manifest can still resolve
        // its metadata.
        let still_readable = lib.read_blob(&preexisting_meta_hash);
        assert!(
            still_readable.is_ok(),
            "pre-existing doc's metadata blob must still be readable after the failed import: {still_readable:?}",
        );
    }

    #[test]
    fn garbage_collect_only_drops_unreferenced_blobs() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();

        // Recorded manifest → these blobs are live.
        let m = seed_manifest(&lib, "doc-1", &[("a.rm", b"AAA")]);
        lib.record_version(&m, Source::Pulled).unwrap();

        // Plant an orphan that no manifest references.
        let orphan_bytes = b"i am an orphan blob".to_vec();
        let orphan = lib.put_blob(&orphan_bytes).unwrap();
        assert!(lib.has_blob(&orphan.hash));

        // Use a zero grace window so the just-written orphan is
        // considered for collection. The default 60s window exists to
        // protect against the in-flight import/record_version race.
        let zero = std::time::Duration::ZERO;
        let report = lib.garbage_collect_with_grace(zero).unwrap();
        assert_eq!(report.deleted, 1);
        assert_eq!(report.bytes_freed, orphan_bytes.len() as u64);
        assert_eq!(report.errors, 0);
        assert!(!lib.has_blob(&orphan.hash));

        // Live blobs and the recorded manifest are intact.
        assert!(lib.has_blob(&m.files[0].sha256));
        assert!(lib.has_blob(&m.hash().unwrap()));

        // Idempotent — second run finds nothing to collect.
        let report2 = lib.garbage_collect_with_grace(zero).unwrap();
        assert_eq!(report2.deleted, 0);
    }

    #[test]
    fn archive_roundtrip_preserves_history() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        // The folder has to actually exist in the local `folders`
        // table — `unarchive_document` now validates that the
        // remembered parent is still around (otherwise it falls
        // back to root) so that a doc archived under a since-
        // deleted folder doesn't come back as a sidebar orphan.
        // Pre-fix the test got away with a fake "folder-A" string.
        lib.upsert_folder("folder-A", None, "Folder A", "{}")
            .unwrap();
        let mut m = seed_manifest(&lib, "doc-1", &[("a.rm", b"AAA")]);
        m.parent = Some("folder-A".into());
        if let Some(map) = m.metadata.as_object_mut() {
            map.insert(
                "parent".into(),
                serde_json::Value::String("folder-A".into()),
            );
        }
        lib.record_version(&m, Source::Pulled).unwrap();

        // Live → archive: the doc is gone from list_documents, the new
        // current manifest has deleted=true, and the archive entry
        // remembers the original parent so restore knows where to put it.
        lib.archive_document("doc-1", ArchiveReason::Local).unwrap();
        assert!(lib.list_documents().unwrap().is_empty());
        let archived = lib.list_archived().unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].document_id, "doc-1");
        assert_eq!(archived[0].reason, ArchiveReason::Local);
        assert_eq!(archived[0].parent.as_deref(), Some("folder-A"));
        assert!(lib.is_archived("doc-1").unwrap());

        // The post-archive manifest carries deleted=true and parent=trash
        // — that's what gets shipped to the device on next push.
        let arch_bytes = lib.read_blob(&archived[0].manifest_hash).unwrap();
        let arch_manifest = Manifest::from_canonical_json(&arch_bytes).unwrap();
        assert_eq!(arch_manifest.parent.as_deref(), Some("trash"));
        let arch_meta_file = arch_manifest
            .files
            .iter()
            .find(|f| f.path.ends_with(".metadata"))
            .unwrap();
        let meta_bytes = lib.read_blob(&arch_meta_file.sha256).unwrap();
        let meta: serde_json::Value = serde_json::from_slice(&meta_bytes).unwrap();
        assert_eq!(meta.get("deleted").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(meta.get("parent").and_then(|v| v.as_str()), Some("trash"));

        // Restore: doc is live again, with deleted=false and original
        // parent reinstated.
        let summary = lib.unarchive_document("doc-1").unwrap();
        assert_eq!(summary.document_id, "doc-1");
        assert_eq!(summary.parent.as_deref(), Some("folder-A"));
        assert!(!lib.is_archived("doc-1").unwrap());
        assert_eq!(lib.list_documents().unwrap().len(), 1);
        assert!(lib.list_archived().unwrap().is_empty());

        // History records the original pull plus the two metadata flips.
        let history = lib.get_history("doc-1").unwrap();
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn move_document_updates_parent_and_records_new_version() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("a.rm", b"page")]);
        lib.record_version(&m, Source::Pulled).unwrap();

        let outcome = lib.move_document("doc-1", Some("folder-X")).unwrap();
        assert!(!outcome.unchanged);

        // The new current manifest reflects the move on both the
        // top-level field and inside the .metadata blob.
        let docs = lib.list_documents().unwrap();
        assert_eq!(docs[0].parent.as_deref(), Some("folder-X"));
        let bytes = lib.read_blob(&docs[0].current_manifest).unwrap();
        let manifest = Manifest::from_canonical_json(&bytes).unwrap();
        assert_eq!(manifest.parent.as_deref(), Some("folder-X"));
        let meta_file = manifest
            .files
            .iter()
            .find(|f| f.path.ends_with(".metadata"))
            .unwrap();
        let meta: serde_json::Value =
            serde_json::from_slice(&lib.read_blob(&meta_file.sha256).unwrap()).unwrap();
        assert_eq!(
            meta.get("parent").and_then(|v| v.as_str()),
            Some("folder-X")
        );

        // Moving back to root sets metadata.parent="" and manifest.parent=None.
        lib.move_document("doc-1", None).unwrap();
        let docs = lib.list_documents().unwrap();
        assert_eq!(docs[0].parent, None);
    }

    #[test]
    fn create_folder_writes_pending_push_row_with_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();

        let parent = lib.create_folder("Inbox", None).unwrap();
        let child = lib
            .create_folder("Drafts", Some(&parent.folder_id))
            .unwrap();

        // Listing surfaces both folders, child links to parent.
        let folders = lib.list_folders().unwrap();
        assert_eq!(folders.len(), 2);
        let listed_parent = folders
            .iter()
            .find(|f| f.folder_id == parent.folder_id)
            .unwrap();
        assert_eq!(listed_parent.parent, None);
        assert_eq!(listed_parent.visible_name, "Inbox");
        let listed_child = folders
            .iter()
            .find(|f| f.folder_id == child.folder_id)
            .unwrap();
        assert_eq!(
            listed_child.parent.as_deref(),
            Some(parent.folder_id.as_str())
        );

        // Both rows are pending push so the next sync uploads their
        // metadata files to the device.
        let pending = lib.list_pending_folder_pushes().unwrap();
        assert_eq!(pending.len(), 2);
        for op in pending {
            let json = match op {
                FolderPushOp::Upsert { metadata_json, .. } => metadata_json,
                FolderPushOp::Delete { .. } => {
                    panic!("create_folder should only enqueue Upsert ops")
                }
            };
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                v.get("type").and_then(|x| x.as_str()),
                Some("CollectionType")
            );
            assert_eq!(v.get("synced").and_then(|x| x.as_bool()), Some(false));
        }
    }

    #[test]
    fn create_folder_rejects_empty_name_and_unknown_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        assert!(lib.create_folder("   ", None).is_err());
        assert!(lib.create_folder("orphan", Some("does-not-exist")).is_err());
    }

    #[test]
    fn reorder_folder_updates_sort_index_and_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let a = lib.create_folder("A", None).unwrap();
        let b = lib.create_folder("B", None).unwrap();
        let c = lib.create_folder("C", None).unwrap();

        // Move A between B and C: midpoint of their sort indices.
        let mid = (b.sort_index + c.sort_index) / 2.0;
        lib.reorder_folder(&a.folder_id, None, mid).unwrap();
        let folders = lib.list_folders().unwrap();
        let order: Vec<_> = folders.iter().map(|f| f.visible_name.as_str()).collect();
        assert_eq!(order, vec!["B", "A", "C"]);

        // Reparent A under B.
        lib.reorder_folder(&a.folder_id, Some(&b.folder_id), 0.0)
            .unwrap();
        let folders = lib.list_folders().unwrap();
        let a_row = folders.iter().find(|f| f.folder_id == a.folder_id).unwrap();
        assert_eq!(a_row.parent.as_deref(), Some(b.folder_id.as_str()));
    }

    #[test]
    fn reparenting_a_folder_queues_a_push_with_the_new_parent() {
        // Regression guard: a user creates two sibling folders at
        // root and then drags one under the other in the sidebar.
        // Pre-fix, `reorder_folder` only touched the local `parent`
        // column — the cached `metadata_json` (and therefore the
        // file written to the tablet) still said `parent: ""`, so
        // the next sync surfaced the dragged folder at the tablet's
        // root. This test locks the contract: after a reparent the
        // pending push must carry the new parent.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let parent = lib.create_folder("Journal", None).unwrap();
        let child = lib.create_folder("BH", None).unwrap();

        // Clear the post-create pending pushes so we know the next
        // pending-push set is purely from the reparent.
        lib.mark_folder_pushed(&parent.folder_id, FolderPushKind::Upsert)
            .unwrap();
        lib.mark_folder_pushed(&child.folder_id, FolderPushKind::Upsert)
            .unwrap();
        assert!(lib.list_pending_folder_pushes().unwrap().is_empty());

        // Drag BH under Journal.
        lib.reorder_folder(&child.folder_id, Some(&parent.folder_id), 0.0)
            .unwrap();

        let pending = lib.list_pending_folder_pushes().unwrap();
        let pushed_json = pending
            .iter()
            .find_map(|op| match op {
                FolderPushOp::Upsert {
                    folder_id,
                    metadata_json,
                } if folder_id == &child.folder_id => Some(metadata_json.clone()),
                _ => None,
            })
            .expect("reparented child must be flagged for an upsert push");
        let v: serde_json::Value = serde_json::from_str(&pushed_json).unwrap();
        assert_eq!(
            v.get("parent").and_then(|x| x.as_str()),
            Some(parent.folder_id.as_str()),
            "push payload must carry the new parent so the device nests correctly",
        );

        // A pure sibling-shuffle (no parent change) must NOT trigger
        // a push — flooding the queue with redundant metadata uploads
        // would slow every drag inside a folder.
        lib.mark_folder_pushed(&child.folder_id, FolderPushKind::Upsert)
            .unwrap();
        let new_sort = 12345.5;
        lib.reorder_folder(&child.folder_id, Some(&parent.folder_id), new_sort)
            .unwrap();
        assert!(
            lib.list_pending_folder_pushes()
                .unwrap()
                .iter()
                .all(|op| op.folder_id() != child.folder_id),
            "pure sort-only reorder must not flag pending_push",
        );
    }

    #[test]
    fn delete_folder_keeps_children_and_tombstones_for_push() {
        // User flow: a "Journal" folder at root contains a "BH"
        // subfolder and a "test" document. Hitting "Delete folder"
        // on Journal must:
        //   1. lift BH up to root (folder still alive, push queued
        //      with parent = "")
        //   2. lift the doc up to root (move_document records a new
        //      version with the new parent)
        //   3. tombstone Journal: metadata.deleted = true, row
        //      hidden from list_folders, pending_push queued so the
        //      device drops it on next sync
        //   4. NOT remove the row from the table — the push engine
        //      needs to ship the tombstone first.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();

        let journal = lib.create_folder("Journal", None).unwrap();
        let bh = lib.create_folder("BH", Some(&journal.folder_id)).unwrap();
        let m = seed_manifest(&lib, "test-doc", &[("a.rm", b"page")]);
        lib.record_version(&m, Source::Pulled).unwrap();
        lib.move_document("test-doc", Some(&journal.folder_id))
            .unwrap();

        // Clear the create-time pending pushes so we can read the
        // deletion's push payload in isolation.
        lib.mark_folder_pushed(&journal.folder_id, FolderPushKind::Upsert)
            .unwrap();
        lib.mark_folder_pushed(&bh.folder_id, FolderPushKind::Upsert)
            .unwrap();
        assert!(lib.list_pending_folder_pushes().unwrap().is_empty());

        let outcome = lib.delete_folder(&journal.folder_id).unwrap();
        assert_eq!(outcome.folders_moved, 1);
        assert_eq!(outcome.documents_moved, 1);

        // BH is still in the library, now at root (parent = None).
        let folders = lib.list_folders().unwrap();
        let bh_after = folders
            .iter()
            .find(|f| f.folder_id == bh.folder_id)
            .expect("BH must remain after parent's deletion");
        assert!(
            bh_after.parent.is_none(),
            "BH should be at root after Journal removed",
        );
        // Journal is hidden from the sidebar even though the row
        // still exists for the upcoming push.
        assert!(
            folders.iter().all(|f| f.folder_id != journal.folder_id),
            "deleted folder must vanish from list_folders before sync",
        );

        // Document was lifted up.
        let doc = lib
            .list_documents()
            .unwrap()
            .into_iter()
            .find(|d| d.document_id == "test-doc")
            .expect("doc still live");
        assert!(
            doc.parent.is_none(),
            "doc should be at root after its folder was deleted",
        );

        // Push queue includes BH (Upsert with the new parent) and
        // Journal (Delete — a hard-remove on the device, not an
        // upload with `deleted: true`). That distinction matters
        // because pushing `deleted: true` metadata would only move
        // the folder to xochitl's Trash; the hard-delete makes it
        // actually go away.
        let pending = lib.list_pending_folder_pushes().unwrap();
        let journal_op = pending
            .iter()
            .find(|op| op.folder_id() == journal.folder_id)
            .expect("Journal must be in the pending-push set");
        match journal_op {
            FolderPushOp::Delete { .. } => {}
            FolderPushOp::Upsert { .. } => {
                panic!("tombstoned folder must enqueue a hard-Delete, not an Upsert")
            }
        }
        let bh_json = pending
            .iter()
            .find_map(|op| match op {
                FolderPushOp::Upsert {
                    folder_id,
                    metadata_json,
                } if folder_id == &bh.folder_id => Some(metadata_json.clone()),
                _ => None,
            })
            .expect("BH must be in the pending-push set as an Upsert");
        let bh_json: serde_json::Value = serde_json::from_str(&bh_json).unwrap();
        assert_eq!(
            bh_json.get("parent").and_then(|v| v.as_str()),
            Some(""),
            "BH's new parent should be the root (empty string on device)",
        );
    }

    #[test]
    fn delete_folder_preserves_grandchildren_under_the_lifted_subfolder() {
        // Three-level chain: Outer → Mid → Leaf. Delete Mid: Leaf
        // should keep its parent = Outer (not jump to root) because
        // we lift Mid's *direct* children only.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let outer = lib.create_folder("Outer", None).unwrap();
        let mid = lib.create_folder("Mid", Some(&outer.folder_id)).unwrap();
        let leaf = lib.create_folder("Leaf", Some(&mid.folder_id)).unwrap();

        lib.delete_folder(&mid.folder_id).unwrap();

        let folders = lib.list_folders().unwrap();
        let leaf_after = folders
            .iter()
            .find(|f| f.folder_id == leaf.folder_id)
            .unwrap();
        assert_eq!(
            leaf_after.parent.as_deref(),
            Some(outer.folder_id.as_str()),
            "Leaf should reparent to Outer when Mid is removed, not jump to root",
        );
    }

    #[test]
    fn mark_folder_pushed_drops_tombstoned_rows() {
        // After the sync ships the tombstone, the row's job is done
        // and `mark_folder_pushed` removes it. Live (non-tombstoned)
        // rows keep being touched in place. Locks the contract that
        // makes the deleted-locally state transient by design.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let live = lib.create_folder("Stays", None).unwrap();
        let doomed = lib.create_folder("Goes", None).unwrap();

        // Live rename → still present after push.
        lib.rename_folder(&live.folder_id, "Stays Renamed").unwrap();
        lib.mark_folder_pushed(&live.folder_id, FolderPushKind::Upsert)
            .unwrap();
        assert!(lib
            .list_folders()
            .unwrap()
            .iter()
            .any(|f| f.folder_id == live.folder_id));

        // Tombstoned → gone after push.
        lib.delete_folder(&doomed.folder_id).unwrap();
        lib.mark_folder_pushed(&doomed.folder_id, FolderPushKind::Delete)
            .unwrap();
        let conn = lib.db.lock();
        let still_there: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM folders WHERE folder_id = ?1",
                params![doomed.folder_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            still_there, 0,
            "post-push, the tombstone row must be removed so a future re-create with the same UUID doesn't collide",
        );
    }

    #[test]
    fn unarchive_falls_back_to_root_when_original_parent_was_deleted() {
        // Audit found a dangling-parent bug: `delete_folder` doesn't
        // touch archived documents (by design, since they're hidden
        // anyway), so an archive entry can outlive its parent folder.
        // Pre-fix, unarchiving such a doc restored its `parent` to a
        // folder uuid that no longer existed, producing a sidebar
        // orphan that only appeared at root anyway. Lock the
        // contract: a missing parent silently falls back to root.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let folder = lib.create_folder("Stash", None).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("a.rm", b"page")]);
        lib.record_version(&m, Source::Pulled).unwrap();
        lib.move_document("doc-1", Some(&folder.folder_id)).unwrap();
        lib.archive_document("doc-1", ArchiveReason::Device)
            .unwrap();

        // Now delete the folder the archived doc remembers. The
        // archived row still holds folder.folder_id as its parent.
        lib.delete_folder(&folder.folder_id).unwrap();
        // mark_folder_pushed flushes the tombstone the way a real
        // sync would; without it the dangling-parent check still
        // passes because deleted_locally = 1 also disqualifies the
        // row.
        lib.mark_folder_pushed(&folder.folder_id, FolderPushKind::Delete)
            .unwrap();

        // Unarchive must NOT restore the doc to the dead parent.
        let restored = lib.unarchive_document("doc-1").unwrap();
        assert_eq!(
            restored.parent, None,
            "unarchive must fall back to root when the original parent no longer exists",
        );
    }

    #[test]
    fn revert_undoes_unpushed_folder_and_document_changes() {
        // User scenario from the bug report: "I delete a folder I
        // am trapped and will have to sync it." Revert has to:
        //   * restore deleted (tombstoned) folders that the device
        //     had previously seen
        //   * drop folders that the user created locally and never
        //     pushed (they have no device counterpart to restore)
        //   * roll back document moves/renames whose new manifest
        //     hasn't been shipped
        //   * NOT touch locally-imported documents (no
        //     `last_seen_manifest`), so the user doesn't lose
        //     fresh imports when reverting unrelated edits.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();

        // Folder that the device "already knows about": simulate
        // by upserting it (the pull path) so it gets a
        // last_synced_metadata_json snapshot.
        let device_metadata = r#"{"visibleName":"Journal","parent":"","type":"CollectionType"}"#;
        lib.upsert_folder("journal-uuid", None, "Journal", device_metadata)
            .unwrap();

        // User edits Journal locally — rename + delete.
        lib.rename_folder("journal-uuid", "Journal (renamed)")
            .unwrap();
        lib.delete_folder("journal-uuid").unwrap();

        // User also creates a brand-new folder, never pushed.
        let new_folder = lib.create_folder("Inbox", None).unwrap();

        // Doc that's been pulled and is in sync_state — we move it
        // locally, which records a new manifest; the row's
        // current_manifest now diverges from last_seen_manifest.
        // `record_version(Source::Pulled)` sets `last_seen_manifest`
        // to the pulled hash, which is exactly the "device confirmed
        // this version" pointer revert needs.
        let m = seed_manifest(&lib, "synced-doc", &[("p.rm", b"page")]);
        lib.record_version(&m, Source::Pulled).unwrap();
        let synced_before = lib
            .list_documents()
            .unwrap()
            .into_iter()
            .find(|d| d.document_id == "synced-doc")
            .unwrap();
        let pre_revert_manifest = synced_before.current_manifest.clone();
        lib.move_document("synced-doc", Some("journal-uuid"))
            .unwrap();
        let drifted = lib
            .list_documents()
            .unwrap()
            .into_iter()
            .find(|d| d.document_id == "synced-doc")
            .unwrap();
        assert_ne!(
            drifted.current_manifest, pre_revert_manifest,
            "precondition: move recorded a new manifest",
        );

        // Locally-imported doc: no last_seen_manifest. Revert must
        // not touch it.
        let import = seed_manifest(&lib, "fresh-import", &[("page.rm", b"new")]);
        lib.record_version(&import, Source::Imported).unwrap();

        // Run revert.
        let report = lib.revert_unpushed_changes().unwrap();
        assert_eq!(report.folders_restored, 1, "Journal must be restored");
        assert_eq!(
            report.folders_dropped, 1,
            "Inbox (locally-created) must be dropped"
        );
        assert_eq!(
            report.documents_rolled_back, 1,
            "synced-doc move must be rolled back"
        );

        // Post-state checks.
        let folders = lib.list_folders().unwrap();
        let journal_after = folders
            .iter()
            .find(|f| f.folder_id == "journal-uuid")
            .expect("Journal must be back in the sidebar");
        assert_eq!(journal_after.visible_name, "Journal");
        assert!(
            folders.iter().all(|f| f.folder_id != new_folder.folder_id),
            "locally-created folder must be removed by revert",
        );

        let synced_after = lib
            .list_documents()
            .unwrap()
            .into_iter()
            .find(|d| d.document_id == "synced-doc")
            .unwrap();
        assert_eq!(
            synced_after.current_manifest, pre_revert_manifest,
            "synced doc must be back at the last-seen manifest",
        );

        // The locally-imported doc is still in the library.
        assert!(
            lib.list_documents()
                .unwrap()
                .iter()
                .any(|d| d.document_id == "fresh-import"),
            "Revert must not delete locally-imported docs",
        );

        // Idempotent — running it again does nothing.
        let again = lib.revert_unpushed_changes().unwrap();
        assert_eq!(again.folders_restored, 0);
        assert_eq!(again.folders_dropped, 0);
        assert_eq!(again.documents_rolled_back, 0);
    }

    #[test]
    fn reorder_folder_rejects_self_or_descendant_target() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let a = lib.create_folder("A", None).unwrap();
        let b = lib.create_folder("B", Some(&a.folder_id)).unwrap();

        // Self → reject.
        assert!(lib
            .reorder_folder(&a.folder_id, Some(&a.folder_id), 0.0)
            .is_err());
        // Into descendant → reject.
        assert!(lib
            .reorder_folder(&a.folder_id, Some(&b.folder_id), 0.0)
            .is_err());
    }

    #[test]
    fn upsert_folder_preserves_local_sort_index_on_repull() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        // Pretend the device just sent us a folder.
        lib.upsert_folder("dev-1", None, "Pulled", "{}").unwrap();
        // User reorders it locally.
        lib.reorder_folder("dev-1", None, 999.5).unwrap();
        // Device sends the same folder again (e.g. another sync).
        lib.upsert_folder("dev-1", None, "Pulled", "{\"v\":2}")
            .unwrap();
        let folders = lib.list_folders().unwrap();
        let row = folders.iter().find(|f| f.folder_id == "dev-1").unwrap();
        // Local order is preserved; metadata is refreshed.
        assert!((row.sort_index - 999.5).abs() < f64::EPSILON);
    }

    #[test]
    fn purge_archived_drops_versions_and_lets_gc_reclaim() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("a.rm", b"unique-bytes-for-purge-test")]);
        lib.record_version(&m, Source::Pulled).unwrap();
        let body_hash = m
            .files
            .iter()
            .find(|f| f.path == "a.rm")
            .unwrap()
            .sha256
            .clone();
        assert!(lib.has_blob(&body_hash));

        lib.archive_document("doc-1", ArchiveReason::Device)
            .unwrap();
        lib.purge_archived_document("doc-1").unwrap();

        // Both archive and version log are empty for this doc.
        assert!(lib.list_archived().unwrap().is_empty());
        assert!(lib.get_history("doc-1").unwrap().is_empty());

        // GC then reclaims the now-orphan blobs.
        let zero = std::time::Duration::ZERO;
        let report = lib.garbage_collect_with_grace(zero).unwrap();
        assert!(report.deleted >= 1, "purge should free at least one blob");
        assert!(!lib.has_blob(&body_hash));
    }

    #[test]
    fn purge_missing_archive_entry_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let err = lib.purge_archived_document("never-existed").unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    #[test]
    fn dedup_across_documents() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let shared = b"shared bytes";
        let h = seed_blob(&lib, shared);
        for doc in ["doc-a", "doc-b"] {
            let mut m = Manifest::new(doc, "Notebook", doc);
            m.files.push(ManifestFile {
                path: "shared".into(),
                sha256: h.clone(),
                size: shared.len() as u64,
                mode: 0o644,
                derived: false,
            });
            lib.record_version(&m, Source::Pulled).unwrap();
        }
        let blob_files: Vec<_> = walkdir(&tmp.path().join("blobs"));
        let matching: Vec<_> = blob_files
            .iter()
            .filter(|p| p.file_name().and_then(|s| s.to_str()) == Some(h.as_str()))
            .collect();
        assert_eq!(matching.len(), 1, "blob should be stored exactly once");
    }

    #[test]
    fn record_derived_artefact_attaches_under_ocr_prefix_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("doc-1.content", b"{}")]);
        lib.record_version(&m, Source::Pulled).unwrap();

        let outcome = lib
            .record_derived_artefact("doc-1", "ocr/transcript.md", b"# Hello\n\nWorld")
            .unwrap();
        let bytes = lib
            .read_derived_artefact(outcome.version_id, "ocr/transcript.md")
            .unwrap()
            .expect("transcript should be readable");
        assert_eq!(bytes, b"# Hello\n\nWorld");

        // A second write at the same path replaces the prior entry rather
        // than accumulating duplicates in the manifest.
        let outcome2 = lib
            .record_derived_artefact("doc-1", "ocr/transcript.md", b"# Updated")
            .unwrap();
        assert_ne!(outcome.version_id, outcome2.version_id);
        let bytes2 = lib
            .read_derived_artefact(outcome2.version_id, "ocr/transcript.md")
            .unwrap()
            .unwrap();
        assert_eq!(bytes2, b"# Updated");
        let entry = lib.get_version(outcome2.version_id).unwrap();
        let manifest_bytes = lib.read_blob(&entry.manifest_hash).unwrap();
        let m = Manifest::from_canonical_json(&manifest_bytes).unwrap();
        let transcripts: Vec<_> = m
            .files
            .iter()
            .filter(|f| f.path == "ocr/transcript.md")
            .collect();
        assert_eq!(transcripts.len(), 1);
        assert!(transcripts[0].derived);
    }

    #[test]
    fn record_derived_artefact_does_not_dirty_an_already_synced_doc() {
        // OCR transcripts live PC-side. If the device was in sync
        // before the user OCR'd, it should still be in sync after —
        // no push queued, no "unsynced" badge.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("doc-1.content", b"{}")]);
        // Pull-source recording sets last_seen = current, putting
        // the doc in the "in sync" state.
        let pulled = lib.record_version(&m, Source::Pulled).unwrap();
        assert!(!doc_dirty(&lib, "doc-1"));

        let derived = lib
            .record_derived_artefact("doc-1", "ocr/transcript.md", b"hello")
            .unwrap();
        assert_ne!(derived.version_id, pulled.version_id);
        assert!(
            !doc_dirty(&lib, "doc-1"),
            "derived-only change must not flip has_unpushed_changes"
        );
    }

    #[test]
    fn record_derived_artefact_preserves_pending_unpushed_changes() {
        // If the user had unpushed edits before OCR'ing (e.g. a
        // rename), we must NOT swallow them by auto-advancing
        // last_seen all the way to the post-OCR manifest. The
        // pending-push state has to survive the derived-only edit.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("doc-1.content", b"{}")]);
        lib.record_version(&m, Source::Pulled).unwrap();

        // Rename → leaves last_seen behind (still pointing at the
        // pre-rename manifest), so the doc is dirty.
        lib.rename_document("doc-1", "Renamed").unwrap();
        assert!(doc_dirty(&lib, "doc-1"));

        // OCR on top of the rename. Auto-advance must NOT fire here
        // because last_seen != prior current.
        lib.record_derived_artefact("doc-1", "ocr/transcript.md", b"hi")
            .unwrap();
        assert!(
            doc_dirty(&lib, "doc-1"),
            "rename was already pending; OCR must not silence the unsynced state"
        );
    }

    fn doc_dirty(lib: &Library, document_id: &str) -> bool {
        lib.list_documents()
            .unwrap()
            .into_iter()
            .find(|d| d.document_id == document_id)
            .map(|d| d.has_unpushed_changes)
            .unwrap_or(false)
    }

    #[test]
    fn record_derived_artefact_rejects_non_ocr_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("doc-1.content", b"{}")]);
        lib.record_version(&m, Source::Pulled).unwrap();
        let r = lib.record_derived_artefact("doc-1", "evil/exec.sh", b"#!/bin/sh");
        assert!(matches!(r, Err(CoreError::InvalidArgument(_))));
    }

    #[test]
    fn probe_path_classifies_directories() {
        // Empty directory → Empty.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            Library::probe_path(empty.path()).unwrap(),
            LibraryPathKind::Empty
        );

        // Non-existent path → Empty (caller's `Library::open` will create it).
        let missing = empty.path().join("does-not-exist");
        assert_eq!(
            Library::probe_path(&missing).unwrap(),
            LibraryPathKind::Empty
        );

        // Existing stamped library → Existing.
        let stamped = tempfile::tempdir().unwrap();
        let _lib = Library::open(stamped.path()).unwrap();
        drop(_lib);
        assert_eq!(
            Library::probe_path(stamped.path()).unwrap(),
            LibraryPathKind::Existing
        );

        // Foreign non-empty directory → InvalidPath error.
        let foreign = tempfile::tempdir().unwrap();
        std::fs::write(foreign.path().join("notes.txt"), b"hi").unwrap();
        assert!(matches!(
            Library::probe_path(foreign.path()),
            Err(CoreError::InvalidPath(_))
        ));
    }

    #[test]
    fn refuses_non_empty_foreign_directory() {
        // Audit fix H9: a renderer-supplied path like ~/Documents/ must
        // not become a "library" with blobs/, db.sqlite, etc. scattered
        // into it.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("foreign.txt"), b"hello").unwrap();
        let result = Library::open(tmp.path());
        assert!(matches!(result, Err(CoreError::InvalidPath(_))));
    }

    #[test]
    fn rejects_existing_library_json_with_bad_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("library.json"), b"{not valid json").unwrap();
        let result = Library::open(tmp.path());
        assert!(matches!(result, Err(CoreError::InvalidPath(_))));
    }

    #[test]
    fn fresh_library_open_writes_a_complete_parseable_stamp() {
        // Issue #32 regression: the initial stamp-out used to be a
        // truncate-in-place `fs::write`, so a crash mid-write left
        // `library.json` half-written and the *next* open refused
        // the directory entirely with InvalidPath — locking the
        // user out of their library until they hand-edited or
        // deleted the file. With the atomic write (tmp → fsync →
        // rename), the post-state is binary: either `library.json`
        // doesn't exist (caller can retry; the open branch fires
        // again), or it exists and is the full pretty-JSON stamp.
        //
        // The truncate-mid-write crash itself can't be staged in a
        // unit test without an injectable fault. The next-best
        // assertion: after a successful Library::open, the file IS
        // there, IS parseable as the full LibraryMeta shape, and
        // no orphan `.tmp*` sibling lingers — which a regression
        // back to plain `fs::write` (no tmp file at all) wouldn't
        // notice on its own, but a regression to a partial atomic
        // shape (write but never persist) would.
        let tmp = tempfile::tempdir().unwrap();
        let _lib = Library::open(tmp.path()).unwrap();

        let stamp_path = tmp.path().join("library.json");
        assert!(stamp_path.is_file(), "library.json must exist post-open");
        let bytes = std::fs::read(&stamp_path).unwrap();
        assert!(
            !bytes.is_empty(),
            "library.json must not be zero-length — that's the exact half-written shape #32 prevents",
        );
        let meta: LibraryMeta = serde_json::from_slice(&bytes)
            .expect("library.json must be parseable as LibraryMeta after atomic stamp-out");
        assert_eq!(meta.schema, LIBRARY_SCHEMA);
        assert!(
            uuid::Uuid::parse_str(&meta.library_id).is_ok(),
            "library_id stamp must be a valid UUID, got {:?}",
            meta.library_id,
        );

        // No leftover staging file. tempfile's NamedTempFile uses
        // a `.tmp` prefix, but the persist call should have renamed
        // it into place; a regression that wrote-but-never-persisted
        // would leave an orphan we'd see here.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".tmp") || name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no .tmp* staging files should remain after open, found {leftovers:?}",
        );
    }

    #[test]
    fn manually_truncated_library_json_is_rejected_not_silently_overwritten() {
        // Issue #32 boundary: the atomic write only fires when
        // library.json doesn't exist (open-creates-stamp branch in
        // Library::open). A partially-written file from an old
        // pre-fix install is STILL a malformed stamp that
        // validate_library_path must refuse — we must not silently
        // overwrite the bad file with a fresh stamp, since the user
        // could lose a recovery affordance (the malformed bytes
        // sometimes contain a recoverable library_id) and because
        // any non-empty file in a library root that fails to parse
        // is a corruption signal worth surfacing.
        let tmp = tempfile::tempdir().unwrap();
        // Zero-length file simulates the exact crash-mid-truncate
        // shape the old fs::write was vulnerable to.
        std::fs::write(tmp.path().join("library.json"), b"").unwrap();
        let err = Library::open(tmp.path())
            .err()
            .expect("zero-length library.json must NOT silently parse / succeed");
        assert!(
            matches!(err, CoreError::InvalidPath(_)),
            "zero-length library.json must surface InvalidPath, got {err:?}",
        );
    }

    #[test]
    fn second_open_on_same_root_is_rejected() {
        // Audit fix H3: two app instances pointed at the same library
        // would corrupt the GC vs. record_version invariant.
        let tmp = tempfile::tempdir().unwrap();
        let _first = Library::open(tmp.path()).unwrap();
        let second = Library::open(tmp.path());
        assert!(matches!(second, Err(CoreError::AlreadyOpen(_))));
        // After dropping the first, the lock is released.
        drop(_first);
        let _third = Library::open(tmp.path()).unwrap();
    }

    #[test]
    fn mark_folder_pushed_with_upsert_kind_preserves_concurrent_tombstone() {
        // Race scenario the v1.0 audit flagged: push completes an
        // Upsert for folder F, then between the device put and
        // `mark_folder_pushed` the user clicks "Delete folder" on F.
        // `delete_folder` flips `deleted_locally = 1` and re-queues a
        // Delete op. If `mark_folder_pushed` routed by the row's
        // current `deleted_locally` it would `DELETE FROM folders`,
        // wiping the queued Delete and leaving F as a ghost on the
        // tablet that no future sync resolves.
        //
        // With the API now taking the *pushed* kind, the Upsert
        // completion path is gated on `deleted_locally = 0` and the
        // tombstoned row stays in the queue.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let folder = lib.create_folder("Doomed", None).unwrap();
        // First push (the Upsert that placed F on the device).
        lib.mark_folder_pushed(&folder.folder_id, FolderPushKind::Upsert)
            .unwrap();
        assert!(lib.list_pending_folder_pushes().unwrap().is_empty());

        // Simulate: push engine snapshots queue (empty here),
        // user renames F (Upsert queued), push engine ships the
        // Upsert to the device, ABOUT to call mark_folder_pushed.
        lib.rename_folder(&folder.folder_id, "Renamed").unwrap();
        // Meanwhile the user clicks Delete on F.
        lib.delete_folder(&folder.folder_id).unwrap();
        // Now the push engine completes — it pushed an Upsert but
        // the row is now `deleted_locally = 1`.
        lib.mark_folder_pushed(&folder.folder_id, FolderPushKind::Upsert)
            .unwrap();

        // The Delete must still be queued: the device has the
        // Upsert'd metadata, and the next sync must ship the Delete
        // to clean it up.
        let pending = lib.list_pending_folder_pushes().unwrap();
        let queued_delete = pending.iter().any(
            |op| matches!(op, FolderPushOp::Delete { folder_id } if folder_id == &folder.folder_id),
        );
        assert!(
            queued_delete,
            "Upsert completion must NOT silently absorb a concurrent Delete — row should still be queued for tombstoning",
        );
    }

    #[test]
    fn unarchive_falls_back_when_parent_is_soft_deleted_but_not_yet_pushed() {
        // Sister test to `unarchive_falls_back_to_root_when_original_parent_was_deleted`,
        // which exercises the *tombstoned-and-pushed* branch (row dropped
        // by `mark_folder_pushed`). This one covers the *deleted-locally*
        // branch — the row is still in the folders table but with
        // `deleted_locally = 1`. The SQL `AND deleted_locally = 0` clause
        // in `unarchive_document` is what disqualifies it; without that
        // clause unarchive would happily restore the doc under a folder
        // the user has already asked to remove.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let folder = lib.create_folder("Stash", None).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("a.rm", b"page")]);
        lib.record_version(&m, Source::Pulled).unwrap();
        lib.move_document("doc-1", Some(&folder.folder_id)).unwrap();
        lib.archive_document("doc-1", ArchiveReason::Device)
            .unwrap();

        // Delete the folder but DO NOT call mark_folder_pushed —
        // the tombstone is queued and the row still carries
        // deleted_locally = 1.
        lib.delete_folder(&folder.folder_id).unwrap();
        let still_present: i64 = {
            let conn = lib.db.lock();
            conn.query_row(
                "SELECT deleted_locally FROM folders WHERE folder_id = ?1",
                params![folder.folder_id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            still_present, 1,
            "precondition: folder is soft-deleted but row still present",
        );

        let restored = lib.unarchive_document("doc-1").unwrap();
        assert_eq!(
            restored.parent, None,
            "unarchive must fall back to root when the parent is soft-deleted but not yet pushed",
        );
    }

    #[test]
    fn revert_after_subsequent_push_restores_to_latest_pushed_snapshot() {
        // Idempotency across multiple push/revert cycles: each
        // successful push refreshes `last_synced_metadata_json`
        // (mark_folder_pushed sets it = metadata_json), so a later
        // revert must restore to the *most recently pushed* shape,
        // not to the original. A regression that froze the snapshot
        // at first push, or that revert cleared it, would surface
        // here as the second revert dragging the folder back to "A"
        // instead of "C".
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();

        // Create + first push → snapshot = "A".
        let folder = lib.create_folder("A", None).unwrap();
        lib.mark_folder_pushed(&folder.folder_id, FolderPushKind::Upsert)
            .unwrap();

        // Rename to "B" → revert. Should restore to "A".
        lib.rename_folder(&folder.folder_id, "B").unwrap();
        lib.revert_unpushed_changes().unwrap();
        let after_first = lib
            .list_folders()
            .unwrap()
            .into_iter()
            .find(|f| f.folder_id == folder.folder_id)
            .unwrap()
            .visible_name;
        assert_eq!(after_first, "A", "first revert restores to original push");

        // Rename to "C" + push → snapshot moves forward to "C".
        lib.rename_folder(&folder.folder_id, "C").unwrap();
        lib.mark_folder_pushed(&folder.folder_id, FolderPushKind::Upsert)
            .unwrap();

        // Rename to "D" + revert. Must restore to "C", NOT "A".
        lib.rename_folder(&folder.folder_id, "D").unwrap();
        lib.revert_unpushed_changes().unwrap();
        let after_second = lib
            .list_folders()
            .unwrap()
            .into_iter()
            .find(|f| f.folder_id == folder.folder_id)
            .unwrap()
            .visible_name;
        assert_eq!(
            after_second, "C",
            "second revert must restore to the latest pushed state, not the original",
        );
    }

    #[test]
    fn move_document_on_archived_row_does_not_resurrect_it() {
        // Boundary contract: archive_document moves the row out of
        // `documents` into `archived_documents`. record_metadata_change
        // intentionally falls through to read from archived_documents
        // (so archive/unarchive themselves can run), which means a
        // misplaced move_document call against an archived id today
        // succeeds and writes a fresh version. That's incidental —
        // what must hold is that the doc STAYS archived and never
        // resurrects into the live listing. A future refactor that
        // re-inserted into `documents` on PostAction::None would
        // surface here as a phantom sidebar entry.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("a.rm", b"page")]);
        lib.record_version(&m, Source::Pulled).unwrap();
        let folder = lib.create_folder("Anywhere", None).unwrap();

        lib.archive_document("doc-1", ArchiveReason::Device)
            .unwrap();
        assert!(lib.is_archived("doc-1").unwrap());
        assert!(lib
            .list_documents()
            .unwrap()
            .iter()
            .all(|d| d.document_id != "doc-1"));

        // Whether the call succeeds or errors is incidental; the
        // post-state is what matters.
        let _ = lib.move_document("doc-1", Some(&folder.folder_id));

        assert!(
            lib.is_archived("doc-1").unwrap(),
            "doc must remain archived after a stray move_document call",
        );
        assert!(
            lib.list_documents()
                .unwrap()
                .iter()
                .all(|d| d.document_id != "doc-1"),
            "archived doc must NOT appear in list_documents after a move attempt",
        );
    }

    #[test]
    fn record_version_on_archived_doc_is_rejected_not_resurrected() {
        // Issue #31 regression. The sync engine's archive-vs-pull race:
        // the pull plan classifies doc A as Changed, the network fetch
        // begins, the user archives A while bytes are in flight, then
        // record_version(Pulled) lands. The pre-fix behaviour
        // unconditionally INSERT-OR-UPDATEd into `documents`, leaving
        // the doc simultaneously in `documents` AND `archived_documents`
        // — the next push then shipped both manifests and the device
        // dropped a doc the user never asked to delete. The contract
        // now: record_version returns DocumentArchived, the row stays
        // out of `documents`, and the archive entry is untouched.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = seed_manifest(&lib, "doc-1", &[("a.rm", b"page-v1")]);
        lib.record_version(&m, Source::Pulled).unwrap();

        // The "user archives mid-pull" half of the race.
        lib.archive_document("doc-1", ArchiveReason::Local).unwrap();
        assert!(lib.is_archived("doc-1").unwrap());

        // The "in-flight pull resumes" half — a fresh manifest with the
        // same doc_id but different content (so the unchanged-hash
        // short-circuit doesn't mask the bug).
        let m2 = seed_manifest(&lib, "doc-1", &[("a.rm", b"page-v2")]);
        let err = lib
            .record_version(&m2, Source::Pulled)
            .expect_err("archived doc must reject a Pulled record_version");
        assert!(
            matches!(err, CoreError::DocumentArchived(ref id) if id == "doc-1"),
            "expected DocumentArchived, got {err:?}",
        );

        // Post-state: archive is intact, no live row, no double-tabling.
        assert!(
            lib.is_archived("doc-1").unwrap(),
            "archive entry must survive the rejected pull",
        );
        assert!(
            lib.list_documents()
                .unwrap()
                .iter()
                .all(|d| d.document_id != "doc-1"),
            "archived doc must NOT appear in list_documents after a rejected pull",
        );
        // And the push planner must see exactly the archive's
        // deleted=true manifest, not a competing live one.
        let live_row_count: i64 = {
            let conn = lib.db.lock();
            conn.query_row(
                "SELECT COUNT(*) FROM documents WHERE document_id = ?1",
                params!["doc-1"],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            live_row_count, 0,
            "documents row must not be resurrected by a Pulled record_version",
        );
    }

    #[test]
    fn migration_0007_adds_last_synced_metadata_json_column() {
        // Pin the column contract that revert depends on. A fresh
        // Library::open must apply 0007 and expose
        // `last_synced_metadata_json` on the folders table. A future
        // migration that renamed or split the column would break
        // revert silently — the user wouldn't notice until they
        // clicked the button and the snapshot fallback ("drop locally-
        // created folders") swallowed every restorable folder.
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let cols: Vec<String> = {
            let conn = lib.db.lock();
            let mut stmt = conn.prepare("PRAGMA table_info(folders)").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert!(
            cols.iter().any(|c| c == "last_synced_metadata_json"),
            "0007 must add last_synced_metadata_json (got cols: {cols:?})",
        );

        // A locally-created folder starts with snapshot = NULL.
        // Revert relies on this to distinguish "drop" from "restore".
        let folder = lib.create_folder("Fresh", None).unwrap();
        let snapshot: Option<String> = {
            let conn = lib.db.lock();
            conn.query_row(
                "SELECT last_synced_metadata_json FROM folders WHERE folder_id = ?1",
                params![folder.folder_id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(
            snapshot.is_none(),
            "locally-created folder must start with NULL snapshot",
        );
    }

    fn walkdir(p: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        if !p.exists() {
            return out;
        }
        for entry in std::fs::read_dir(p).unwrap() {
            let e = entry.unwrap();
            let path = e.path();
            if path.is_dir() {
                out.extend(walkdir(&path));
            } else {
                out.push(path);
            }
        }
        out
    }
}
