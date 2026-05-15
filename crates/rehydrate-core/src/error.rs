use thiserror::Error;

/// The crate's error type. Named `CoreError` (rather than bare
/// `Error`) so the prefix matches the workspace convention used by
/// every other crate (`SyncError`, `DeviceError`, `OcrError`,
/// `PublishError`, `HttpError`, `ParseError`). Call sites that
/// import a list of error types from across the workspace can then
/// see at a glance which crate each came from.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("serde_json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("library at {path} is corrupt: {reason}")]
    Corrupt { path: String, reason: String },

    #[error("manifest references unknown blob {0}")]
    MissingBlob(String),

    /// A blob's bytes on disk no longer hash to the filename the
    /// content-addressed store filed them under. Either the store
    /// is corrupted (bit-rot, FS truncation) or the file was
    /// modified out-of-band. Surfaced from `read_to_vec` so the
    /// hot path never silently serves tampered bytes to push,
    /// OCR, or reconstruct.
    #[error("blob {expected} corrupt: bytes hash to {actual}")]
    BlobCorrupt { expected: String, actual: String },

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid library path: {0}")]
    InvalidPath(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("library at {0} is already open by another process")]
    AlreadyOpen(String),

    /// A reconstruction/export was asked to write to a path that
    /// already exists, with `allow_overwrite = false`. The library
    /// returns this rather than silently clobbering the file.
    #[error("destination already exists: {0}")]
    AlreadyExists(String),

    /// Caller tried to record a new live version for a document that
    /// is currently in `archived_documents`. The sync engine treats
    /// this as a per-document skip (the user's archive wins over the
    /// in-flight pull); unarchive first to re-record. See library.rs
    /// `record_version` for the archive-vs-pull race this guards.
    #[error("document {0} is archived; unarchive before recording a new version")]
    DocumentArchived(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
