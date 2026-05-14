//! Filesystem-backed fake of the device. Mirrors the xochitl layout so
//! tests, UI development, and CI runs can exercise every code path without
//! a tablet plugged in.
//!
//! Layout under `root`:
//! ```text
//! <root>/
//!   <uuid>.metadata        # JSON; `type` field: "DocumentType" | "CollectionType",
//!                          # `visibleName`, `parent`, `lastModified`, ...
//!   <uuid>.content         # JSON; per-document content metadata
//!   <uuid>.pagedata        # plain text, optional
//!   <uuid>/                # directory of page rm files (optional)
//!   <uuid>/<page-uuid>.rm
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{DeviceError, DeviceResult};
use crate::model::{DeviceInfo, RemoteEntry, RemoteEntryKind, RemoteFile};
use crate::trait_def::Device;

pub struct FakeDevice {
    root: PathBuf,
    info: DeviceInfo,
}

impl FakeDevice {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            info: DeviceInfo {
                model: "FakeDevice".to_string(),
                serial: Some("fake-0001".to_string()),
                software_version: Some("0.0.0".to_string()),
            },
        }
    }

    fn metadata_path(&self, uuid: &str) -> PathBuf {
        self.root.join(format!("{uuid}.metadata"))
    }
}

#[async_trait]
impl Device for FakeDevice {
    async fn ping(&self) -> DeviceResult<DeviceInfo> {
        if !self.root.exists() {
            return Err(DeviceError::Unreachable(self.root.display().to_string()));
        }
        Ok(self.info.clone())
    }

    async fn list_documents(&self) -> DeviceResult<Vec<RemoteEntry>> {
        if !self.root.exists() {
            return Err(DeviceError::Unreachable(self.root.display().to_string()));
        }
        let mut out = Vec::new();
        let mut rd = tokio::fs::read_dir(&self.root).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(uuid) = name.strip_suffix(".metadata") else {
                continue;
            };

            let bytes = tokio::fs::read(&path).await?;
            let metadata: Value = serde_json::from_slice(&bytes)
                .map_err(|e| DeviceError::Protocol(format!("bad metadata json for {uuid}: {e}")))?;
            let visible_name = metadata
                .get("visibleName")
                .and_then(|v| v.as_str())
                .unwrap_or("Untitled")
                .to_string();
            let parent = metadata
                .get("parent")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let device_mtime_hint = metadata
                .get("lastModified")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let type_field = metadata.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let kind = if type_field == "CollectionType" {
                RemoteEntryKind::Folder
            } else {
                RemoteEntryKind::Document
            };
            // Phase 1 derives doc_type from the .content json's fileType.
            let content_path = self.root.join(format!("{uuid}.content"));
            let doc_type = if let Ok(cb) = tokio::fs::read(&content_path).await {
                let cv: Value = serde_json::from_slice(&cb).unwrap_or(Value::Null);
                cv.get("fileType")
                    .and_then(|v| v.as_str())
                    .map(|s| match s {
                        "pdf" => "DocumentType.Pdf".to_string(),
                        "epub" => "DocumentType.Epub".to_string(),
                        _ => "Notebook".to_string(),
                    })
                    .unwrap_or_else(|| "Notebook".to_string())
            } else {
                match kind {
                    RemoteEntryKind::Folder => "Folder".to_string(),
                    RemoteEntryKind::Document => "Notebook".to_string(),
                }
            };

            out.push(RemoteEntry {
                uuid: uuid.to_string(),
                visible_name,
                doc_type,
                parent,
                kind,
                device_mtime_hint,
                metadata,
            });
        }
        out.sort_by(|a, b| a.uuid.cmp(&b.uuid));
        Ok(out)
    }

    async fn put_document_tree(&self, uuid: &str, files: &[RemoteFile]) -> DeviceResult<()> {
        let desired: HashSet<String> = files.iter().map(|f| f.path.clone()).collect();
        for f in files {
            let target = self.root.join(&f.path);
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // Atomic write: temp + rename in the same directory.
            let tmp = target.with_extension(format!(
                "{}.tmp",
                target.extension().and_then(|s| s.to_str()).unwrap_or("rm")
            ));
            tokio::fs::write(&tmp, &f.bytes).await?;
            tokio::fs::rename(&tmp, &target).await?;
        }
        // Issue #22: `put_document_tree` is a true replace — any
        // pre-existing `<uuid>*` artefact that's NOT in the new
        // manifest must be removed. Without this, restoring an older
        // version (or pushing one with removed pages/sidecars) leaves
        // stale `.rm` / `.pagedata` / thumbnail files for xochitl, and
        // a later pull re-ingests them and corrupts the restored
        // version. Mirrors `SshDevice::put_document_tree`.
        reap_extras(&self.root, uuid, &desired).await
    }

    async fn delete_document_tree(&self, uuid: &str) -> DeviceResult<()> {
        // Mirror the SshDevice impl: idempotently remove every
        // `<uuid>*` artefact at the root, recursing into per-uuid
        // directories first so their parent's rmdir succeeds.
        // NotFound is idempotent (already gone), but any other error
        // must propagate so the folder push queue keeps the tombstone
        // — issue #23.
        let prefix = format!("{uuid}.");
        let mut read = match tokio::fs::read_dir(&self.root).await {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let mut first_err: Option<DeviceError> = None;
        while let Some(entry) = read.next_entry().await? {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str != uuid && !name_str.starts_with(&prefix) {
                continue;
            }
            let path = entry.path();
            let meta = match tokio::fs::symlink_metadata(&path).await {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e.into());
                    }
                    continue;
                }
            };
            let res = if meta.is_dir() {
                tokio::fs::remove_dir_all(&path).await
            } else {
                tokio::fs::remove_file(&path).await
            };
            if let Err(e) = res {
                if e.kind() != std::io::ErrorKind::NotFound && first_err.is_none() {
                    first_err = Some(e.into());
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn fetch_document_tree(&self, uuid: &str) -> DeviceResult<Vec<RemoteFile>> {
        let metadata_path = self.metadata_path(uuid);
        if !metadata_path.exists() {
            return Err(DeviceError::NotFound(uuid.to_string()));
        }
        let mut out = Vec::new();
        // Top-level associated files: <uuid>.metadata, <uuid>.content, <uuid>.pagedata, etc.
        let mut rd = tokio::fs::read_dir(&self.root).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.starts_with(&format!("{uuid}.")) {
                let bytes = tokio::fs::read(&path).await?;
                out.push(RemoteFile {
                    path: name.to_string(),
                    bytes,
                    mode: 0o644,
                });
            }
        }
        // Optional per-document directory of page files.
        let dir = self.root.join(uuid);
        if dir.exists() {
            collect_dir(&dir, &dir, &mut out, uuid).await?;
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
}

/// Remove every `<uuid>*` artefact under `root` that isn't in `desired`.
/// Returns the first non-NotFound error encountered, mirroring the
/// SSH path's "first error wins" reporting so the caller can surface
/// it without losing its kind.
async fn reap_extras(root: &Path, uuid: &str, desired: &HashSet<String>) -> DeviceResult<()> {
    let prefix = format!("{uuid}.");
    let mut rd = match tokio::fs::read_dir(root).await {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut first_err: Option<DeviceError> = None;
    let mut subdirs: Vec<(PathBuf, String)> = Vec::new();
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        if name_str != uuid && !name_str.starts_with(&prefix) {
            continue;
        }
        let path = entry.path();
        let meta = match tokio::fs::symlink_metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e.into());
                }
                continue;
            }
        };
        if meta.is_dir() {
            subdirs.push((path, name_str));
        } else if !desired.contains(&name_str) {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                if e.kind() != std::io::ErrorKind::NotFound && first_err.is_none() {
                    first_err = Some(e.into());
                }
            }
        }
    }
    for (dir_path, rel_root) in subdirs {
        match reap_subtree(&dir_path, &rel_root, desired).await {
            Ok(true) => {
                if let Err(e) = tokio::fs::remove_dir(&dir_path).await {
                    if e.kind() != std::io::ErrorKind::NotFound && first_err.is_none() {
                        first_err = Some(e.into());
                    }
                }
            }
            Ok(false) => {}
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Recurse into a `<uuid>*` directory, deleting files not in `desired`
/// and removing now-empty subdirectories. Returns `true` when the
/// directory itself ended up empty, so the caller can rmdir it.
async fn reap_subtree(dir: &Path, rel_root: &str, desired: &HashSet<String>) -> DeviceResult<bool> {
    let mut rd = tokio::fs::read_dir(dir).await?;
    let mut remaining = 0usize;
    let mut first_err: Option<DeviceError> = None;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        let rel_path = format!("{rel_root}/{name_str}");
        let meta = match tokio::fs::symlink_metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e.into());
                }
                remaining += 1;
                continue;
            }
        };
        if meta.is_dir() {
            match Box::pin(reap_subtree(&path, &rel_path, desired)).await {
                Ok(true) => {
                    if let Err(e) = tokio::fs::remove_dir(&path).await {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            if first_err.is_none() {
                                first_err = Some(e.into());
                            }
                            remaining += 1;
                        }
                    }
                }
                Ok(false) => {
                    remaining += 1;
                }
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                    remaining += 1;
                }
            }
        } else if desired.contains(&rel_path) {
            remaining += 1;
        } else if let Err(e) = tokio::fs::remove_file(&path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                if first_err.is_none() {
                    first_err = Some(e.into());
                }
                remaining += 1;
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(remaining == 0),
    }
}

async fn collect_dir(
    base: &Path,
    cur: &Path,
    out: &mut Vec<RemoteFile>,
    uuid: &str,
) -> DeviceResult<()> {
    let mut rd = tokio::fs::read_dir(cur).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            Box::pin(collect_dir(base, &path, out, uuid)).await?;
        } else {
            let bytes = tokio::fs::read(&path).await?;
            let rel = path
                .strip_prefix(base.parent().unwrap_or(base))
                .unwrap_or(&path);
            // We want paths like "<uuid>/<page>.rm".
            let rel_str = format!("{uuid}/{}", rel.strip_prefix(uuid).unwrap_or(rel).display());
            out.push(RemoteFile {
                path: rel_str,
                bytes,
                mode: 0o644,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn lists_documents_and_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("doc-1.metadata"),
            r#"{"visibleName":"Hello","type":"DocumentType","parent":""}"#,
        )
        .unwrap();
        fs::write(root.join("doc-1.content"), r#"{"fileType":"pdf"}"#).unwrap();
        fs::write(
            root.join("folder-1.metadata"),
            r#"{"visibleName":"My folder","type":"CollectionType","parent":""}"#,
        )
        .unwrap();
        let dev = FakeDevice::new(root);
        let entries = dev.list_documents().await.unwrap();
        assert_eq!(entries.len(), 2);
        let doc = entries.iter().find(|e| e.uuid == "doc-1").unwrap();
        assert_eq!(doc.kind, RemoteEntryKind::Document);
        assert_eq!(doc.doc_type, "DocumentType.Pdf");
        let folder = entries.iter().find(|e| e.uuid == "folder-1").unwrap();
        assert_eq!(folder.kind, RemoteEntryKind::Folder);
    }

    #[tokio::test]
    async fn put_document_tree_reaps_extras_not_in_manifest() {
        // Issue #22 regression: a previous push left `<uuid>*` artefacts
        // (extra sidecars, an extra page file, a stale thumbnail) on the
        // device. Re-pushing with a smaller manifest must remove every
        // artefact not present in the new file set; otherwise xochitl
        // keeps serving the stale files and the next pull re-ingests
        // them.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Pre-seed a "previous version" of doc-1 with an extra sidecar,
        // an extra page, and an extra thumbnail.
        fs::write(root.join("doc-1.metadata"), b"OLD-METADATA").unwrap();
        fs::write(root.join("doc-1.content"), b"OLD-CONTENT").unwrap();
        fs::write(root.join("doc-1.pagedata"), b"OLD-PAGEDATA").unwrap();
        fs::create_dir(root.join("doc-1")).unwrap();
        fs::write(root.join("doc-1").join("page-a.rm"), b"OLD-A").unwrap();
        fs::write(root.join("doc-1").join("page-stale.rm"), b"OLD-STALE").unwrap();
        fs::create_dir(root.join("doc-1.thumbnails")).unwrap();
        fs::write(
            root.join("doc-1.thumbnails").join("page-stale.jpg"),
            b"OLD-THUMB",
        )
        .unwrap();
        // An unrelated document — must be untouched by a doc-1 push.
        fs::write(root.join("doc-2.metadata"), b"OTHER").unwrap();

        let dev = FakeDevice::new(root);
        let new_files = vec![
            RemoteFile {
                path: "doc-1.metadata".into(),
                bytes: b"NEW-METADATA".to_vec(),
                mode: 0o644,
            },
            RemoteFile {
                path: "doc-1.content".into(),
                bytes: b"NEW-CONTENT".to_vec(),
                mode: 0o644,
            },
            RemoteFile {
                path: "doc-1/page-a.rm".into(),
                bytes: b"NEW-A".to_vec(),
                mode: 0o644,
            },
        ];
        dev.put_document_tree("doc-1", &new_files).await.unwrap();

        // Files in the new manifest land with the new bytes.
        assert_eq!(
            fs::read(root.join("doc-1.metadata")).unwrap(),
            b"NEW-METADATA"
        );
        assert_eq!(
            fs::read(root.join("doc-1.content")).unwrap(),
            b"NEW-CONTENT"
        );
        assert_eq!(
            fs::read(root.join("doc-1").join("page-a.rm")).unwrap(),
            b"NEW-A"
        );

        // Stale artefacts not in the new manifest are gone.
        assert!(!root.join("doc-1.pagedata").exists());
        assert!(!root.join("doc-1").join("page-stale.rm").exists());
        assert!(!root.join("doc-1.thumbnails").exists());

        // Unrelated document is untouched.
        assert_eq!(fs::read(root.join("doc-2.metadata")).unwrap(), b"OTHER");
    }

    #[tokio::test]
    async fn fetch_document_tree_includes_associated_files_and_pages() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(
            root.join("doc-1.metadata"),
            r#"{"visibleName":"Notebook","type":"DocumentType"}"#,
        )
        .unwrap();
        fs::write(root.join("doc-1.content"), r#"{"fileType":"notebook"}"#).unwrap();
        fs::create_dir(root.join("doc-1")).unwrap();
        fs::write(root.join("doc-1").join("page-a.rm"), b"AAA").unwrap();
        fs::write(root.join("doc-1").join("page-b.rm"), b"BBB").unwrap();

        let dev = FakeDevice::new(root);
        let files = dev.fetch_document_tree("doc-1").await.unwrap();
        let paths: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
        assert!(paths.contains(&"doc-1.metadata".to_string()));
        assert!(paths.contains(&"doc-1.content".to_string()));
        assert!(paths.contains(&"doc-1/page-a.rm".to_string()));
        assert!(paths.contains(&"doc-1/page-b.rm".to_string()));
    }
}
