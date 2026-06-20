//! High-level Library facade.
//!
//! Phase 1 implements a lean subset: open an empty library, store blobs,
//! record a manifest as a new version. Phase 2 will add reconstruct + history
//! UI surfaces; Phase 4 will add GC. The data model already supports them.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use fs4::FileExt;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::blob::BlobStore;
use crate::db::Db;
use crate::error::{CoreError, Result};
use crate::hash::Sha256Hex;
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

mod archive;
mod blobs;
mod derived;
mod documents;
mod folders;
mod import;
mod maintenance;
mod versions;

#[cfg(test)]
mod tests;

// `ReconstructOptions` is defined alongside `reconstruct_with` in
// `maintenance`; re-export it here so external callers reach it at the
// stable path `rehydrate_core::library::ReconstructOptions` (and via
// the crate-root re-export in `lib.rs`).
pub use maintenance::ReconstructOptions;

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
}
