//! Content-addressed blob store.
//!
//! Blobs are stored under `<library>/blobs/<aa>/<bb>/<sha256>` where `<aa>` and
//! `<bb>` are the first two pairs of hex characters of the sha256 — a 2-byte
//! fanout. Writes go through `<library>/tmp/` and are atomically renamed into
//! place. Blobs are immutable; a duplicate `put` is a no-op.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;

use crate::error::{CoreError, Result};
use crate::hash::{Sha256Hex, StreamingHasher};
use crate::paths::LibraryPaths;

pub struct BlobStore {
    paths: LibraryPaths,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The blob was newly written.
    Stored,
    /// The blob was already present; the caller's input was discarded.
    Deduplicated,
}

#[derive(Debug, Clone)]
pub struct PutResult {
    pub hash: Sha256Hex,
    pub size: u64,
    pub outcome: PutOutcome,
}

impl BlobStore {
    pub fn new(paths: LibraryPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &LibraryPaths {
        &self.paths
    }

    pub fn has(&self, hash: &Sha256Hex) -> bool {
        self.paths.blob_path(hash).exists()
    }

    /// Store bytes into the blob store. Returns the hash, size, and whether
    /// the blob was newly written or deduplicated against an existing entry.
    pub fn put_bytes(&self, bytes: &[u8]) -> Result<PutResult> {
        self.put_reader(&mut std::io::Cursor::new(bytes))
    }

    /// Stream a reader into the blob store. The reader is consumed exactly
    /// once; on the dedup path we still consume it to avoid surprising the
    /// caller (they passed us bytes; we hashed them).
    pub fn put_reader<R: Read>(&self, reader: &mut R) -> Result<PutResult> {
        // Stage to <tmp>/<random>; we don't yet know the hash, so we can't
        // place it directly under blobs/.
        fs::create_dir_all(&self.paths.tmp)?;
        let staging = tempfile::NamedTempFile::new_in(&self.paths.tmp)?;
        let (staging_file, staging_path) = staging.into_parts();
        let mut writer = io::BufWriter::new(staging_file);
        let mut hasher = StreamingHasher::new();

        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            writer.write_all(&buf[..n])?;
        }
        writer.flush()?;
        let inner = writer
            .into_inner()
            .map_err(|e| CoreError::Io(io::Error::other(e.to_string())))?;
        inner.sync_all()?;
        drop(inner);

        let (hash, size) = hasher.finish();
        let final_path = self.paths.blob_path(&hash);

        if final_path.exists() {
            // Already present; discard staged copy.
            let _ = fs::remove_file(&staging_path);
            return Ok(PutResult {
                hash,
                size,
                outcome: PutOutcome::Deduplicated,
            });
        }

        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Atomic rename. On the same filesystem this is atomic on all
        // platforms we support. tempfile's NamedTempFile drop would unlink
        // the path, so we keep the PathBuf and call rename ourselves.
        match fs::rename(&staging_path, &final_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // Race: another writer landed first. Discard our copy.
                let _ = fs::remove_file(&staging_path);
                return Ok(PutResult {
                    hash,
                    size,
                    outcome: PutOutcome::Deduplicated,
                });
            }
            Err(e) => {
                let _ = fs::remove_file(&staging_path);
                return Err(e.into());
            }
        }

        // Issue #30: fsync every directory level from the blob's
        // parent up to (and including) the blob store root. POSIX
        // guarantees `fsync(file)` makes the file's bytes durable,
        // but a newly-created directory entry pointing at that file
        // is only durable after `fsync` of the *containing*
        // directory — recursively, because each level may itself be
        // a fresh create_dir_all from this very call (the 2-byte
        // fanout shard `aa/bb/` is materialised lazily on the first
        // blob landing there). Without this walk, a power loss
        // between `fs::rename` and the kernel's eventual page-cache
        // flush can leave the manifest (committed to SQLite at
        // synchronous=NORMAL, so it IS durable) pointing at a hash
        // whose directory entry never made it to disk; the user
        // sees `MissingBlob` on next read with no clear recovery.
        //
        // Best-effort (the file bytes are already durable from
        // `inner.sync_all()` above; this just shortens the window
        // in which the *directory entry* lags behind), matching
        // the pre-existing best-effort convention for the
        // immediate parent.
        for dir in directories_to_fsync(final_path.parent(), &self.paths.blobs) {
            if let Ok(f) = File::open(dir) {
                let _ = f.sync_all();
            }
        }

        Ok(PutResult {
            hash,
            size,
            outcome: PutOutcome::Stored,
        })
    }

    pub fn open(&self, hash: &Sha256Hex) -> Result<File> {
        let p = self.paths.blob_path(hash);
        File::open(&p).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => CoreError::MissingBlob(hash.to_string()),
            _ => CoreError::Io(e),
        })
    }

    pub fn read_to_vec(&self, hash: &Sha256Hex) -> Result<Vec<u8>> {
        let mut f = self.open(hash)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        // Verify the bytes still hash to the filename. Catches
        // bit-rot, FS truncation, and out-of-band modification of
        // the blob file. The cost is one SHA-256 pass per read
        // (~500 MB/s on Apple Silicon); the alternative — trusting
        // the filesystem to never corrupt content-addressed storage
        // — would silently feed garbled bytes to push (re-poisoning
        // the tablet), OCR, and reconstruct. v1.0 audit flagged
        // the unverified-read path as a data-integrity hazard.
        let actual = Sha256Hex::from_bytes(&buf);
        if actual.as_str() != hash.as_str() {
            return Err(CoreError::BlobCorrupt {
                expected: hash.to_string(),
                actual: actual.to_string(),
            });
        }
        Ok(buf)
    }

    pub fn path_for(&self, hash: &Sha256Hex) -> PathBuf {
        self.paths.blob_path(hash)
    }
}

/// Return every directory level from `start` up to (and including)
/// `root`, in walk-up order. Returns an empty `Vec` if `start` is
/// None or doesn't lie under `root` — defensive, since a runaway
/// walk past the blob store would be a bug. Pure / pathbuf-only so
/// the termination behaviour can be exercised without touching the
/// filesystem.
fn directories_to_fsync(start: Option<&std::path::Path>, root: &std::path::Path) -> Vec<PathBuf> {
    let Some(start) = start else {
        return Vec::new();
    };
    if !start.starts_with(root) {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(4);
    let mut cursor: &std::path::Path = start;
    loop {
        out.push(cursor.to_path_buf());
        if cursor == root {
            break;
        }
        let Some(parent) = cursor.parent() else { break };
        cursor = parent;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tmp: &tempfile::TempDir) -> BlobStore {
        let paths = LibraryPaths::new(tmp.path());
        paths.ensure_dirs().unwrap();
        BlobStore::new(paths)
    }

    #[test]
    fn put_get_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let bs = store(&tmp);
        let r = bs.put_bytes(b"hello world").unwrap();
        assert_eq!(r.outcome, PutOutcome::Stored);
        assert_eq!(r.size, 11);
        assert!(bs.has(&r.hash));
        assert_eq!(bs.read_to_vec(&r.hash).unwrap(), b"hello world");
    }

    #[test]
    fn put_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let bs = store(&tmp);
        let a = bs.put_bytes(b"same").unwrap();
        let b = bs.put_bytes(b"same").unwrap();
        assert_eq!(a.hash, b.hash);
        assert_eq!(b.outcome, PutOutcome::Deduplicated);
    }

    #[test]
    fn distinct_inputs_distinct_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let bs = store(&tmp);
        let a = bs.put_bytes(b"alpha").unwrap();
        let b = bs.put_bytes(b"beta").unwrap();
        assert_ne!(a.hash, b.hash);
    }

    /// Issue #30 regression: the durability walk must cover the
    /// blob's parent up through (and including) the blob-store
    /// root. The hash `aabbcc…` lives at `blobs/aa/bb/<hash>`, so
    /// the walk must yield exactly three directories: the leaf
    /// shard, its parent shard, and the blob root. A regression
    /// that stopped one level short would let the freshly-created
    /// `bb/` entry in `aa/` (or the `aa/` entry in `blobs/`) lag
    /// behind a kernel crash and orphan the blob.
    #[test]
    fn directories_to_fsync_covers_full_chain_inclusive() {
        let root = PathBuf::from("/lib/blobs");
        let leaf = PathBuf::from("/lib/blobs/aa/bb");
        let dirs = super::directories_to_fsync(Some(leaf.as_path()), &root);
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/lib/blobs/aa/bb"),
                PathBuf::from("/lib/blobs/aa"),
                PathBuf::from("/lib/blobs"),
            ],
            "walk must yield leaf, mid, root — in that order",
        );
    }

    /// Issue #30 safety: the walk must STOP at the blob-store root.
    /// Without the sentinel check, an off-by-one would step out of
    /// `blobs/` into the library root, and from there toward `/`,
    /// fsyncing directories the blob store doesn't own. None of
    /// those are correctness-load-bearing, but the loop would burn
    /// extra fsyncs and (worse) imply the wrong invariant to a
    /// future reader.
    #[test]
    fn directories_to_fsync_does_not_escape_root() {
        let root = PathBuf::from("/lib/blobs");
        let leaf = PathBuf::from("/lib/blobs/aa/bb");
        let dirs = super::directories_to_fsync(Some(leaf.as_path()), &root);
        for d in &dirs {
            assert!(
                d.starts_with(&root),
                "{d:?} escaped the blob-store root {root:?}",
            );
        }
    }

    /// Issue #30: defensive — if for any reason the blob's parent
    /// is not under the blob root (shouldn't happen, since
    /// `blob_path` always composes a path under `paths.blobs`),
    /// the walk must return an empty list rather than fsyncing
    /// arbitrary ancestors of an unrelated path.
    #[test]
    fn directories_to_fsync_refuses_paths_outside_root() {
        let root = PathBuf::from("/lib/blobs");
        let stray = PathBuf::from("/etc/passwd.d");
        let dirs = super::directories_to_fsync(Some(stray.as_path()), &root);
        assert!(
            dirs.is_empty(),
            "walk on a path outside the blob root must yield nothing, got {dirs:?}",
        );
    }

    #[test]
    fn read_to_vec_detects_on_disk_corruption() {
        // Regression guard against the v1.0 audit's data-integrity
        // finding: pre-fix, reads trusted the filename without
        // verifying that the bytes inside still hashed to it.
        // Bit-rot, FS truncation, or out-of-band tampering would
        // be silently fed to push (re-poisoning the tablet), OCR,
        // and reconstruct.
        let tmp = tempfile::tempdir().unwrap();
        let bs = store(&tmp);
        let r = bs.put_bytes(b"original contents").unwrap();
        // Tamper with the file on disk.
        let path = bs.path_for(&r.hash);
        std::fs::write(&path, b"tampered contents").unwrap();
        // The hashes don't match, so read_to_vec must refuse.
        let err = bs.read_to_vec(&r.hash);
        assert!(
            matches!(err, Err(CoreError::BlobCorrupt { .. })),
            "tampered blob must surface BlobCorrupt, got {err:?}",
        );
    }
}
