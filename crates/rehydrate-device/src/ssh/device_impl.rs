//! `impl Device for SshDevice` and the file-local subtree helpers
//! (`fetch_subtree*`, `reap_extras`, `reap_subtree`) that only this
//! impl uses.

use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;
use russh_sftp::client::SftpSession;
use serde_json::Value;

use crate::error::{DeviceError, DeviceResult};
use crate::model::{DeviceInfo, RemoteEntry, RemoteEntryKind, RemoteFile};
use crate::trait_def::Device;

use super::io::{read_path, sftp_err, with_timeout, MAX_SUBTREE_DEPTH};
use super::SshDevice;

#[async_trait]
impl Device for SshDevice {
    async fn ping(&self) -> DeviceResult<DeviceInfo> {
        let model = self
            .exec("cat /sys/devices/soc0/machine 2>/dev/null || echo reMarkable")
            .await?;
        // /proc/device-tree/serial-number is NUL-terminated; trim NULs and ws.
        let serial = self
            .exec("tr -d '\\0' < /proc/device-tree/serial-number 2>/dev/null || true")
            .await
            .ok()
            .map(|s| s.trim().to_string());
        // reMarkable firmware exposes the release in /usr/share/remarkable/update.conf
        // (REMARKABLE_RELEASE_VERSION=...). Fall back to /etc/version if missing.
        let software_version = self
            .exec(
                "(awk -F= '/^REMARKABLE_RELEASE_VERSION/ {print $2}' \
                  /usr/share/remarkable/update.conf 2>/dev/null; \
                  cat /etc/version 2>/dev/null) | head -n1",
            )
            .await
            .ok()
            .map(|s| s.trim().to_string());
        Ok(DeviceInfo {
            model: if model.is_empty() {
                "reMarkable".into()
            } else {
                model
            },
            serial: serial.filter(|s| !s.is_empty()),
            software_version: software_version.filter(|s| !s.is_empty()),
        })
    }

    async fn list_documents(&self) -> DeviceResult<Vec<RemoteEntry>> {
        let inner = self.inner.lock().await;
        let dir = self.cfg.xochitl_dir.clone();
        let entries = with_timeout("list_documents.read_dir", async {
            inner
                .sftp
                .read_dir(&dir)
                .await
                .map_err(|e| sftp_err("read_dir", &dir, e))
        })
        .await?;

        let mut out = Vec::new();
        for entry in entries {
            let name = entry.file_name();
            // Issue #33: defence-in-depth at the SFTP boundary. A
            // device that returned `../etc/passwd.metadata` would
            // strip-suffix cleanly to `../etc/passwd` and we'd then
            // open `{xochitl}/../etc/passwd.metadata` — outside the
            // document tree. See `crate::is_safe_entry_name`.
            if !crate::is_safe_entry_name(&name) {
                tracing::warn!(
                    parent = %dir,
                    entry = %name,
                    "device sent a non-leaf entry name in list_documents; skipping (issue #33)",
                );
                continue;
            }
            let Some(uuid) = name.strip_suffix(".metadata") else {
                continue;
            };

            let metadata_path = format!("{dir}/{name}");
            let bytes = self.read_file(&inner.sftp, &metadata_path).await?;
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
                .filter(|s| !s.is_empty())
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

            // Best-effort doc_type from <uuid>.content's fileType.
            let doc_type = if matches!(kind, RemoteEntryKind::Document) {
                let content_path = format!("{dir}/{uuid}.content");
                match self.read_file(&inner.sftp, &content_path).await {
                    Ok(cb) => {
                        let cv: Value = serde_json::from_slice(&cb).unwrap_or(Value::Null);
                        cv.get("fileType")
                            .and_then(|v| v.as_str())
                            .map(|s| match s {
                                "pdf" => "DocumentType.Pdf".to_string(),
                                "epub" => "DocumentType.Epub".to_string(),
                                _ => "Notebook".to_string(),
                            })
                            .unwrap_or_else(|| "Notebook".to_string())
                    }
                    Err(_) => "Notebook".to_string(),
                }
            } else {
                "Folder".to_string()
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
        let inner = self.inner.lock().await;
        let dir = self.cfg.xochitl_dir.clone();

        // Phase 1: stage every file as `<path>.rehydrate-tmp`. The live
        // document on the device is not touched yet, so an upload failure
        // in this phase leaves it intact.
        let mut targets: Vec<String> = Vec::with_capacity(files.len());
        for f in files {
            let target = format!("{dir}/{}", f.path);
            if let Err(e) = self.stage_file(&inner.sftp, &target, &f.bytes).await {
                self.discard_staged(&inner.sftp, &targets).await;
                return Err(e);
            }
            targets.push(target);
        }

        // Phase 2: promote each staged file into place. Per-file commit
        // uses a backup pattern so a single failed promote can be rolled
        // back. A failure here can leave the document partially-updated;
        // we surface the error so the caller doesn't advance sync_state
        // and the next push will retry the whole tree.
        for target in &targets {
            if let Err(e) = self.commit_staged(&inner.sftp, target).await {
                // Best-effort: try to discard remaining staged files so
                // the device isn't littered with stale .rehydrate-tmp.
                self.discard_staged(&inner.sftp, &targets).await;
                return Err(e);
            }
        }

        // Phase 3: issue #22 — `put_document_tree` is a true replace.
        // Enumerate any pre-existing `<uuid>*` artefact on the device
        // that is NOT in the new manifest and remove it. Without this,
        // restoring an older version (or pushing one with removed pages
        // / sidecars) leaves stale `.rm` / `.pagedata` / thumbnail
        // files in xochitl, and a later pull re-ingests them and
        // corrupts the restored version. Returning Err keeps sync_state
        // un-advanced so the next push retries the reap; both staging
        // and reaping are idempotent.
        reap_extras(&inner.sftp, &dir, uuid, &desired).await?;

        drop(inner);

        // Per-document restart was removed: a multi-document push
        // would otherwise restart xochitl N times, blanking the
        // tablet UI for ~3s each. The push engine batches a single
        // `refresh_document_index` call after all docs land.
        Ok(())
    }

    async fn delete_document_tree(&self, uuid: &str) -> DeviceResult<()> {
        let inner = self.inner.lock().await;
        let dir = self.cfg.xochitl_dir.clone();
        let prefix = format!("{uuid}.");
        // Enumerate the xochitl root and collect anything matching
        // `<uuid>.*`. Files inside the per-uuid subdir are walked
        // separately by `discard_subtree` below — the directory entry
        // itself shows up as a `<uuid>` (no extension) in the listing
        // and is removed last, after its contents.
        let entries = with_timeout("delete_document_tree.read_dir", async {
            inner
                .sftp
                .read_dir(&dir)
                .await
                .map_err(|e| sftp_err("read_dir", &dir, e))
        })
        .await?;

        // Gather the leaves first; remove subdir contents before the
        // subdir itself, otherwise SFTP rmdir fails with ENOTEMPTY.
        let mut sibling_files: Vec<String> = Vec::new();
        let mut sibling_dirs: Vec<String> = Vec::new();
        for e in entries {
            let name = e.file_name();
            // Issue #33: refuse non-leaf entry names from the SFTP
            // server. Without this gate a `uuid.../etc/shadow` entry
            // would pass the prefix check below (`starts_with(prefix)`)
            // and the `remove_file` loop would attempt destructive
            // ops outside the document tree. See
            // `crate::is_safe_entry_name`.
            if !crate::is_safe_entry_name(&name) {
                tracing::warn!(
                    parent = %dir,
                    entry = %name,
                    "device sent a non-leaf entry name in delete_document_tree; skipping (issue #33)",
                );
                continue;
            }
            // Match both the bare uuid (the per-document directory)
            // and any `<uuid>.<ext>` sidecar (`.metadata`, `.content`,
            // `.pagedata`, `.local`, `.thumbnails/`, …).
            if name != uuid && !name.starts_with(&prefix) {
                continue;
            }
            let path = format!("{dir}/{name}");
            if e.file_type().is_dir() {
                sibling_dirs.push(path);
            } else {
                sibling_files.push(path);
            }
        }

        // Track the first non-NotFound failure so the caller treats
        // the delete as still pending. NotFound is idempotent — the
        // tablet may have GC'd the artefact, or the row may never
        // have been pushed — but anything else (transient SFTP error,
        // permission denied, timeout) must surface so the folder push
        // queue keeps the tombstone for the next sync. A swallowed
        // error here was the root cause of issue #23: a failed
        // SFTP remove was reported as Ok, mark_folder_pushed cleared
        // the tombstone, and the next pull resurrected the folder.
        let mut first_err: Option<DeviceError> = None;
        let record = |slot: &mut Option<DeviceError>, err: DeviceError| {
            if !matches!(err, DeviceError::NotFound(_)) && slot.is_none() {
                *slot = Some(err);
            }
        };

        for path in &sibling_files {
            if let Err(e) = with_timeout("delete_document_tree.remove_file", async {
                inner
                    .sftp
                    .remove_file(path)
                    .await
                    .map_err(|e| sftp_err("remove_file", path, e))
            })
            .await
            {
                record(&mut first_err, e);
            }
        }

        for dir_path in &sibling_dirs {
            // Clean the directory's contents, then remove the
            // directory itself. xochitl per-uuid dirs are typically
            // one level deep (page files, thumbnails), so a single
            // read_dir + remove pass is enough.
            let inner_entries = match with_timeout("delete_document_tree.read_subdir", async {
                inner
                    .sftp
                    .read_dir(dir_path)
                    .await
                    .map_err(|e| sftp_err("read_dir", dir_path, e))
            })
            .await
            {
                Ok(es) => Some(es),
                Err(e) => {
                    // NotFound here means the per-uuid dir vanished
                    // between the listing and the read — fine, fall
                    // through to the rmdir below which will also
                    // NotFound. Anything else gets recorded.
                    record(&mut first_err, e);
                    None
                }
            };
            for e in inner_entries.into_iter().flatten() {
                let name = e.file_name();
                // Issue #33: defence-in-depth at the SFTP boundary.
                // The pre-existing `.`/`..` filter handled the
                // obvious cases; tightening to a full leaf-component
                // check catches embedded slashes / backslashes too.
                if !crate::is_safe_entry_name(&name) {
                    tracing::warn!(
                        parent = %dir_path,
                        entry = %name,
                        "device sent a non-leaf entry name in delete subdir; skipping (issue #33)",
                    );
                    continue;
                }
                let path = format!("{dir_path}/{name}");
                if let Err(err) = with_timeout("delete_document_tree.remove_subfile", async {
                    inner
                        .sftp
                        .remove_file(&path)
                        .await
                        .map_err(|err| sftp_err("remove_file", &path, err))
                })
                .await
                {
                    record(&mut first_err, err);
                }
            }
            if let Err(e) = with_timeout("delete_document_tree.remove_dir", async {
                inner
                    .sftp
                    .remove_dir(dir_path)
                    .await
                    .map_err(|e| sftp_err("remove_dir", dir_path, e))
            })
            .await
            {
                record(&mut first_err, e);
            }
        }

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn refresh_document_index(&self) -> DeviceResult<()> {
        // The tablet caches the document index in memory; this
        // restart is what makes freshly-pushed files visible on the
        // tablet's UI. ~3s interruption. If it fails we return Err so
        // the push engine can emit a warning event — the files
        // themselves are already safely on the device.
        self.exec("systemctl restart xochitl").await.map(|_| ())
    }

    async fn fetch_document_tree(&self, uuid: &str) -> DeviceResult<Vec<RemoteFile>> {
        let inner = self.inner.lock().await;
        let dir = self.cfg.xochitl_dir.clone();

        let metadata_path = format!("{dir}/{uuid}.metadata");
        // Probe; an open() error on .metadata means "not found".
        let _ = self.read_file(&inner.sftp, &metadata_path).await?;

        let mut out = Vec::new();
        // 1. All sibling files prefixed with `<uuid>.`
        let entries = with_timeout("fetch_document_tree.read_dir", async {
            inner
                .sftp
                .read_dir(&dir)
                .await
                .map_err(|e| sftp_err("read_dir", &dir, e))
        })
        .await?;
        for entry in entries {
            let name = entry.file_name();
            // Issue #33: defence-in-depth at the SFTP boundary.
            // Without this gate, a device that returned an entry
            // like `<uuid>.../etc/passwd` would pass the
            // `starts_with(uuid.)` prefix check below, and
            // `read_path({dir}/<uuid>.../etc/passwd)` would read
            // off-tree on the device AND smuggle the bad path
            // into `RemoteFile.path` for the manifest.
            if !crate::is_safe_entry_name(&name) {
                tracing::warn!(
                    parent = %dir,
                    entry = %name,
                    "device sent a non-leaf entry name in fetch_document_tree; skipping (issue #33)",
                );
                continue;
            }
            if !name.starts_with(&format!("{uuid}.")) {
                continue;
            }
            // Skip subdirs under xochitl/ — handled below.
            if entry.file_type().is_dir() {
                continue;
            }
            // Audit fix H1: a previously-failed push leaves staging
            // tombstones (`*.rehydrate-tmp`, `*.rehydrate-bak`) on
            // the device. Without this filter we'd hash them as if
            // they were real document files, store them in the
            // manifest, and round-trip them back on the next push.
            if name.ends_with(".rehydrate-tmp") || name.ends_with(".rehydrate-bak") {
                continue;
            }
            let path = format!("{dir}/{name}");
            let bytes = self.read_file(&inner.sftp, &path).await?;
            out.push(RemoteFile {
                path: name.clone(),
                bytes,
                mode: 0o644,
            });
        }

        // 2. Optional per-document directory: <xochitl>/<uuid>/...
        // The directory is genuinely optional — some document types
        // don't have one — so a NotFound here is benign and must not
        // be allowed to skip the whole document. Route through
        // sftp_err so the "what counts as not-found" rule lives in
        // exactly one place.
        let subdir = format!("{dir}/{uuid}");
        match inner.sftp.read_dir(&subdir).await {
            Ok(_) => {
                fetch_subtree(&inner.sftp, &subdir, uuid, &mut out).await?;
            }
            Err(e) => match sftp_err("read_dir", &subdir, e) {
                DeviceError::NotFound(_) => {}
                other => return Err(other),
            },
        }

        // 3. Optional thumbnails directory: <xochitl>/<uuid>.thumbnails
        // Same shape as #2: NotFound is benign (not every document
        // has thumbnails) but any other error must propagate. The
        // previous `if let Ok(_)` swallowed every error indiscriminately,
        // so a transient SFTP failure here would silently drop the
        // thumbnails subtree and the document would still be recorded
        // as a successful sync.
        let thumb = format!("{dir}/{uuid}.thumbnails");
        match inner.sftp.read_dir(&thumb).await {
            Ok(_) => {
                fetch_subtree_named(&inner.sftp, &thumb, &format!("{uuid}.thumbnails"), &mut out)
                    .await?;
            }
            Err(e) => match sftp_err("read_dir", &thumb, e) {
                DeviceError::NotFound(_) => {}
                other => return Err(other),
            },
        }

        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
}

/// Recursively read everything under `device_dir` into `out`, with on-disk
/// paths rewritten so the output uses `<uuid>/<rel...>` as the path.
async fn fetch_subtree(
    sftp: &SftpSession,
    device_dir: &str,
    uuid: &str,
    out: &mut Vec<RemoteFile>,
) -> DeviceResult<()> {
    fetch_subtree_named(sftp, device_dir, uuid, out).await
}

async fn fetch_subtree_named(
    sftp: &SftpSession,
    device_dir: &str,
    rel_root: &str,
    out: &mut Vec<RemoteFile>,
) -> DeviceResult<()> {
    // (device path, relative path, depth-from-root). Depth is bounded so a
    // pathological tree — or a symlink that the SFTP server does not
    // advertise as such — cannot trap us indefinitely.
    let mut stack: Vec<(String, PathBuf, usize)> =
        vec![(device_dir.to_string(), PathBuf::from(rel_root), 0)];
    while let Some((dev, rel, depth)) = stack.pop() {
        let entries = with_timeout("fetch_subtree.read_dir", async {
            sftp.read_dir(&dev)
                .await
                .map_err(|e| sftp_err("read_dir", &dev, e))
        })
        .await?;
        for entry in entries {
            let name = entry.file_name();
            // Issue #33: defence-in-depth at the SFTP boundary.
            // Without this gate, a device that returned an entry
            // like `../../etc/shadow` would have us `sftp.open()`
            // that path (escaping xochitl_dir) and smuggle it into
            // the manifest's `RemoteFile.path`. The symlink check
            // below catches one shape of escape; this catches the
            // other.
            if !crate::is_safe_entry_name(&name) {
                tracing::warn!(
                    parent = %dev,
                    entry = %name,
                    "device sent a non-leaf entry name in fetch_subtree; skipping (issue #33)",
                );
                continue;
            }
            // Skip symlinks: the device reports `xochitl` straight off the
            // stock filesystem and should not contain symlinks under a
            // document tree, so anything that does is either malicious
            // (loop back to `..`) or accidental (some debug helper).
            // Either way we can't safely follow it.
            if entry.file_type().is_symlink() {
                continue;
            }
            let dev_child = format!("{dev}/{name}");
            let rel_child = rel.join(&name);
            if entry.file_type().is_dir() {
                if depth + 1 > MAX_SUBTREE_DEPTH {
                    return Err(DeviceError::Other(format!(
                        "subtree depth exceeds {MAX_SUBTREE_DEPTH} at {dev_child}"
                    )));
                }
                stack.push((dev_child, rel_child, depth + 1));
            } else {
                let bytes = read_path(sftp, &dev_child).await?;
                out.push(RemoteFile {
                    path: rel_child.to_string_lossy().replace('\\', "/"),
                    bytes,
                    mode: 0o644,
                });
            }
        }
    }
    Ok(())
}

/// Issue #22: remove every `<uuid>*` artefact under `xochitl_dir` that
/// is NOT in the `desired` set, so `put_document_tree` behaves as a
/// true replace. Mirrors `FakeDevice::reap_extras`. The first
/// non-NotFound failure is returned so the caller can leave sync state
/// un-advanced and retry on the next push.
async fn reap_extras(
    sftp: &SftpSession,
    xochitl_dir: &str,
    uuid: &str,
    desired: &HashSet<String>,
) -> DeviceResult<()> {
    let prefix = format!("{uuid}.");
    let entries = with_timeout("reap_extras.read_dir", async {
        sftp.read_dir(xochitl_dir)
            .await
            .map_err(|e| sftp_err("read_dir", xochitl_dir, e))
    })
    .await?;

    let mut first_err: Option<DeviceError> = None;
    let mut subdirs: Vec<(String, String)> = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        if name != uuid && !name.starts_with(&prefix) {
            continue;
        }
        let path = format!("{xochitl_dir}/{name}");
        if entry.file_type().is_dir() {
            subdirs.push((path, name));
        } else if !desired.contains(&name) {
            if let Err(e) = with_timeout("reap_extras.remove_file", async {
                sftp.remove_file(&path)
                    .await
                    .map_err(|e| sftp_err("remove_file", &path, e))
            })
            .await
            {
                if !matches!(e, DeviceError::NotFound(_)) && first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    for (dir_path, rel_root) in subdirs {
        match reap_subtree(sftp, &dir_path, &rel_root, desired, 0).await {
            Ok(true) => {
                if let Err(e) = with_timeout("reap_extras.remove_dir", async {
                    sftp.remove_dir(&dir_path)
                        .await
                        .map_err(|e| sftp_err("remove_dir", &dir_path, e))
                })
                .await
                {
                    if !matches!(e, DeviceError::NotFound(_)) && first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
            Ok(false) => {}
            Err(e) => {
                if !matches!(e, DeviceError::NotFound(_)) && first_err.is_none() {
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
/// and rmdir-ing now-empty subdirectories. Returns `true` when this
/// directory itself is empty and the caller can rmdir it. Bounded by
/// [`MAX_SUBTREE_DEPTH`] to mirror `fetch_subtree_named`.
async fn reap_subtree(
    sftp: &SftpSession,
    dev_dir: &str,
    rel_root: &str,
    desired: &HashSet<String>,
    depth: usize,
) -> DeviceResult<bool> {
    if depth > MAX_SUBTREE_DEPTH {
        return Err(DeviceError::Other(format!(
            "reap_subtree depth exceeds {MAX_SUBTREE_DEPTH} at {dev_dir}"
        )));
    }
    let entries = with_timeout("reap_subtree.read_dir", async {
        sftp.read_dir(dev_dir)
            .await
            .map_err(|e| sftp_err("read_dir", dev_dir, e))
    })
    .await?;
    let mut remaining = 0usize;
    let mut first_err: Option<DeviceError> = None;
    for entry in entries {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let dev_path = format!("{dev_dir}/{name}");
        let rel_path = format!("{rel_root}/{name}");
        if entry.file_type().is_dir() {
            match Box::pin(reap_subtree(sftp, &dev_path, &rel_path, desired, depth + 1)).await {
                Ok(true) => {
                    if let Err(e) = with_timeout("reap_subtree.remove_dir", async {
                        sftp.remove_dir(&dev_path)
                            .await
                            .map_err(|e| sftp_err("remove_dir", &dev_path, e))
                    })
                    .await
                    {
                        if !matches!(e, DeviceError::NotFound(_)) {
                            if first_err.is_none() {
                                first_err = Some(e);
                            }
                            remaining += 1;
                        }
                    }
                }
                Ok(false) => {
                    remaining += 1;
                }
                Err(e) => {
                    if !matches!(e, DeviceError::NotFound(_)) && first_err.is_none() {
                        first_err = Some(e);
                    }
                    remaining += 1;
                }
            }
        } else if desired.contains(&rel_path) {
            remaining += 1;
        } else if let Err(e) = with_timeout("reap_subtree.remove_file", async {
            sftp.remove_file(&dev_path)
                .await
                .map_err(|e| sftp_err("remove_file", &dev_path, e))
        })
        .await
        {
            if !matches!(e, DeviceError::NotFound(_)) {
                if first_err.is_none() {
                    first_err = Some(e);
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
