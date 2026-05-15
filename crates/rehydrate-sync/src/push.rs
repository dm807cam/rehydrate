//! Push planning and execution.
//!
//! A push is the mirror of a pull: every document whose `current_manifest`
//! differs from `sync_state.last_seen_manifest` gets reconstructed from the
//! blob store and uploaded via `Device::put_document_tree`.
//!
//! Documents that have no `sync_state` row are *library-only* — typically
//! imports — and are skipped here because Phase 3 has no `delete_document`
//! semantics on the device side yet. Phase 4 will handle imports.

use rehydrate_core::{DocumentSummary, Library, Manifest};
use rehydrate_device::{Device, RemoteFile};
use serde::{Deserialize, Serialize};

use crate::error::{SyncError, SyncResult};
use crate::execute::Cancel;
use crate::progress::{Progress, ProgressEvent};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PushItemStatus {
    Outbound,
    Unchanged,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushItem {
    pub document: DocumentSummary,
    pub status: PushItemStatus,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushPlan {
    pub items: Vec<PushItem>,
}

pub fn plan_push(library: &Library) -> SyncResult<PushPlan> {
    // Includes archived documents whose new (deleted=true) manifest still
    // needs to reach the device. After the push, last_seen_manifest gets
    // updated and they classify as Unchanged on the next plan_push.
    let docs = library.list_pushable_documents()?;
    let mut items = Vec::with_capacity(docs.len());
    for doc in docs {
        let last_seen = library.last_seen(&doc.document_id)?;
        let item = match last_seen.and_then(|(_, m)| m) {
            // Library and device manifest agree → nothing to push.
            Some(last_manifest) if last_manifest == doc.current_manifest.as_str() => PushItem {
                document: doc,
                status: PushItemStatus::Unchanged,
                reason: None,
            },
            // Library has a different manifest (restored or updated) — push.
            // OR: no last_seen_manifest at all, meaning the document was
            // imported and has never been synced. Either way it's outbound.
            // The device-side put_document_tree call creates new files just
            // as readily as it overwrites existing ones.
            _ => PushItem {
                document: doc,
                status: PushItemStatus::Outbound,
                reason: None,
            },
        };
        items.push(item);
    }
    Ok(PushPlan { items })
}

#[derive(Debug, Clone)]
pub struct PushReport {
    pub pushed: usize,
    pub unchanged: usize,
    pub skipped: usize,
}

pub async fn execute_push(
    library: &Library,
    device: &dyn Device,
    plan: PushPlan,
    progress: Option<Progress>,
    cancel: Cancel,
) -> SyncResult<PushReport> {
    let total = plan.items.len();
    if let Some(p) = &progress {
        let _ = p
            .send(ProgressEvent::PlanReady {
                total_documents: total,
            })
            .await;
    }

    let mut pushed = 0usize;
    let mut unchanged = 0usize;
    let mut skipped = 0usize;

    for item in plan.items {
        if cancel.is_cancelled() {
            if let Some(p) = &progress {
                let _ = p.send(ProgressEvent::Cancelled).await;
            }
            return Err(SyncError::Cancelled);
        }
        match item.status {
            PushItemStatus::Unchanged => {
                unchanged += 1;
                continue;
            }
            PushItemStatus::Skipped => {
                skipped += 1;
                if let Some(p) = &progress {
                    let _ = p
                        .send(ProgressEvent::DocumentSkipped {
                            document_id: item.document.document_id.clone(),
                            reason: item.reason.clone().unwrap_or_default(),
                        })
                        .await;
                }
                continue;
            }
            PushItemStatus::Outbound => {}
        }

        if let Some(p) = &progress {
            let _ = p
                .send(ProgressEvent::DocumentStarted {
                    document_id: item.document.document_id.clone(),
                    visible_name: item.document.visible_name.clone(),
                })
                .await;
        }

        match push_one(library, device, &item, progress.as_ref()).await {
            Ok(()) => {
                if let Some(p) = &progress {
                    let _ = p
                        .send(ProgressEvent::DocumentCompleted {
                            document_id: item.document.document_id.clone(),
                            unchanged: false,
                        })
                        .await;
                }
                pushed += 1;
            }
            Err(e) => {
                tracing::warn!(uuid = %item.document.document_id, error = %e, "push skipped");
                skipped += 1;
                if let Some(p) = &progress {
                    let _ = p
                        .send(ProgressEvent::DocumentSkipped {
                            document_id: item.document.document_id.clone(),
                            reason: e.to_string(),
                        })
                        .await;
                }
            }
        }
    }

    // After document push, flush any folder operations. Renames /
    // reparents / creations upload the folder's `<uuid>.metadata`;
    // deletions hard-remove every `<uuid>*` artefact on the device
    // so xochitl drops the folder outright instead of moving it to
    // its Trash view (which is what `deleted: true` in metadata
    // would do). Either shape ends with `mark_folder_pushed` so the
    // local row is reconciled (cleared, or dropped for tombstones).
    //
    // Failures here are logged and counted as skips so a single
    // broken folder doesn't block the rest of the queue. Audit fix
    // M3: propagate DB errors instead of treating them as "no
    // pending folders" — silently skipping a folder op used to make
    // the user think the sync succeeded.
    let pending_folder_ops = library.list_pending_folder_pushes()?;
    for op in pending_folder_ops {
        if cancel.is_cancelled() {
            break;
        }
        let folder_id = op.folder_id().to_string();
        // Capture kind from the queue snapshot, NOT from the row's
        // current `deleted_locally`. A concurrent `delete_folder`
        // can flip the row's state between this push and the
        // mark_folder_pushed below; routing from the snapshot keeps
        // the device-side reconciliation honest. (See the comment on
        // `Library::mark_folder_pushed` for the ghost-folder bug
        // that motivates this.)
        let pushed_kind = op.kind();
        let push_result = match op {
            rehydrate_core::FolderPushOp::Upsert {
                ref folder_id,
                ref metadata_json,
            } => {
                let file = rehydrate_device::RemoteFile {
                    path: format!("{folder_id}.metadata"),
                    bytes: metadata_json.clone().into_bytes(),
                    mode: 0o644,
                };
                device.put_document_tree(folder_id, &[file]).await
            }
            rehydrate_core::FolderPushOp::Delete { ref folder_id } => {
                device.delete_document_tree(folder_id).await
            }
        };
        match push_result {
            Ok(()) => {
                // Audit fix M2: the mark_folder_pushed failure path
                // used to log-and-continue, so the same folder op
                // re-pushed forever. Count it as skipped instead so
                // the user sees a non-zero skip count and the loop
                // doesn't claim success.
                if let Err(e) = library.mark_folder_pushed(&folder_id, pushed_kind) {
                    tracing::warn!(
                        folder = %folder_id,
                        error = %e,
                        "could not clear folder pending_push (will retry next sync)"
                    );
                    skipped += 1;
                } else {
                    pushed += 1;
                }
            }
            Err(e) => {
                tracing::warn!(folder = %folder_id, error = %e, "folder push failed");
                skipped += 1;
            }
        }
    }

    // Refresh the tablet's document index once for the whole push
    // session. Per-document restarts would blank the UI for several
    // seconds each. A failure here means the files are safely on
    // the device but the tablet UI may keep showing the old state
    // until the user reboots — surface a warning event so the UI
    // can tell the user, but don't fail the sync (the next push
    // will retry the refresh).
    if pushed > 0 && !cancel.is_cancelled() {
        if let Err(e) = device.refresh_document_index().await {
            tracing::warn!(error = %e, "post-push index refresh failed");
            if let Some(p) = &progress {
                let _ = p
                    .send(ProgressEvent::Warning {
                        message: format!(
                            "Files uploaded successfully, but the tablet's document index \
                             didn't refresh. Reboot the tablet, or it will pick up the \
                             changes on the next sync. ({e})"
                        ),
                    })
                    .await;
            }
        }
    }

    if let Some(p) = &progress {
        let _ = p
            .send(ProgressEvent::Done {
                recorded: pushed,
                unchanged,
                skipped,
            })
            .await;
    }
    Ok(PushReport {
        pushed,
        unchanged,
        skipped,
    })
}

async fn push_one(
    library: &Library,
    device: &dyn Device,
    item: &PushItem,
    progress: Option<&Progress>,
) -> SyncResult<()> {
    let manifest_bytes = library.read_blob(&item.document.current_manifest)?;
    let manifest = Manifest::from_canonical_json(&manifest_bytes)?;

    let mut files = Vec::with_capacity(manifest.files.len());
    for f in &manifest.files {
        // Skip library-side derived artefacts (OCR transcripts, etc.).
        // They live in the manifest so they version + restore + GC
        // cleanly, but they don't belong on the tablet's xochitl
        // file index.
        if f.derived {
            continue;
        }
        let bytes = library.read_blob(&f.sha256)?;
        let size = bytes.len() as u64;
        if let Some(p) = progress {
            let _ = p
                .send(ProgressEvent::FileFetched {
                    document_id: item.document.document_id.clone(),
                    file: f.path.clone(),
                    bytes: size,
                    deduped: false,
                })
                .await;
        }
        files.push(RemoteFile {
            path: f.path.clone(),
            bytes,
            mode: f.mode,
        });
    }

    device
        .put_document_tree(&item.document.document_id, &files)
        .await?;

    // Files are now on the device. We MUST advance last_seen_manifest
    // or the next push will treat this doc as outbound again and
    // re-upload — overwriting any device-side edit the user makes in
    // the interim. Transient SQLite errors (busy, locked) get a few
    // retries with backoff so we don't lose the device-side state
    // because of momentary contention. If every retry fails the
    // error propagates: the caller logs it and the next push will
    // re-run this branch, which is safe because both the device
    // write and the DB update are idempotent.
    let doc_id = &item.document.document_id;
    let manifest_hex = item.document.current_manifest.as_str();
    let mut last_err = None;
    for attempt in 0..5u32 {
        match library.update_last_seen_manifest(doc_id, manifest_hex) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                // Exponential-ish backoff: 25, 50, 100, 200, 400 ms.
                let delay_ms = 25u64 << attempt;
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }
    }
    Err(last_err.expect("loop ran at least once").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rehydrate_core::manifest::ManifestFile;
    use rehydrate_core::{Manifest, Sha256Hex, Source};
    use rehydrate_device::fake::FakeDevice;

    fn seed_doc(lib: &Library, doc_id: &str, name: &str, files: &[(&str, &[u8])]) {
        let mut m = Manifest::new(doc_id, "Notebook", name);
        for (path, bytes) in files {
            let res = lib.put_blob(bytes).unwrap();
            m.files.push(ManifestFile {
                path: (*path).to_string(),
                sha256: res.hash,
                size: res.size,
                mode: 0o644,
                derived: false,
            });
        }
        lib.record_version(&m, Source::Pulled).unwrap();
        // Record sync_state so plan_push doesn't classify as Skipped.
        lib.update_last_seen_manifest(
            doc_id,
            Sha256Hex::from_bytes(&m.canonical_json().unwrap()).as_str(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn push_uploads_changed_documents() {
        let lib_dir = tempfile::tempdir().unwrap();
        let dev_dir = tempfile::tempdir().unwrap();
        let lib = Library::open(lib_dir.path()).unwrap();
        let dev = FakeDevice::new(dev_dir.path());

        seed_doc(
            &lib,
            "doc-1",
            "Doc One",
            &[("doc-1.metadata", b"{}"), ("doc-1.content", b"{}")],
        );

        // First plan: nothing to push — last_seen matches current.
        let plan = plan_push(&lib).unwrap();
        assert!(plan
            .items
            .iter()
            .all(|i| i.status == PushItemStatus::Unchanged));

        // Restore (no-op since same manifest)... so simulate a change by
        // recording a new version on top.
        let mut m = Manifest::new("doc-1", "Notebook", "Doc One renamed");
        let res = lib.put_blob(b"{\"renamed\":true}").unwrap();
        m.files.push(ManifestFile {
            path: "doc-1.metadata".into(),
            sha256: res.hash,
            size: res.size,
            mode: 0o644,
            derived: false,
        });
        let res2 = lib.put_blob(b"{}").unwrap();
        m.files.push(ManifestFile {
            path: "doc-1.content".into(),
            sha256: res2.hash,
            size: res2.size,
            mode: 0o644,
            derived: false,
        });
        lib.record_version(&m, Source::Restored).unwrap();

        let plan = plan_push(&lib).unwrap();
        let outbound = plan
            .items
            .iter()
            .filter(|i| i.status == PushItemStatus::Outbound)
            .count();
        assert_eq!(outbound, 1);

        let report = execute_push(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();
        assert_eq!(report.pushed, 1);

        // The fake device now has the renamed metadata.
        let on_device = std::fs::read(dev_dir.path().join("doc-1.metadata")).unwrap();
        assert_eq!(on_device, b"{\"renamed\":true}");

        // Re-planning shows nothing outbound.
        let plan = plan_push(&lib).unwrap();
        assert!(plan
            .items
            .iter()
            .all(|i| i.status == PushItemStatus::Unchanged));
    }

    #[tokio::test]
    async fn folder_delete_op_hard_removes_metadata_from_device() {
        // Regression guard for commit 24ea264. A folder delete must
        // route the push through Device::delete_document_tree
        // (sweeping every `<uuid>*` artefact off the device) rather
        // than uploading metadata with `deleted: true`. The latter
        // would only move the folder to xochitl's Trash view; the
        // user would still see it on the tablet until they emptied
        // trash there manually. The core test in rehydrate-core
        // verifies the queue enqueues Delete (not Upsert); this one
        // pins what actually happens on the wire.
        let lib_dir = tempfile::tempdir().unwrap();
        let dev_dir = tempfile::tempdir().unwrap();
        let lib = Library::open(lib_dir.path()).unwrap();
        let dev = FakeDevice::new(dev_dir.path());

        // Create + first push → metadata lands on the fake device.
        let folder = lib.create_folder("Journal", None).unwrap();
        let plan = plan_push(&lib).unwrap();
        execute_push(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();
        let metadata_path = dev_dir
            .path()
            .join(format!("{}.metadata", folder.folder_id));
        assert!(
            metadata_path.exists(),
            "precondition: folder metadata must be on device after first push",
        );
        // Stray sibling artefact under the same uuid prefix proves
        // the delete sweep takes everything, not just .metadata.
        let stray = dev_dir.path().join(format!("{}.stray", folder.folder_id));
        std::fs::write(&stray, b"garbage").unwrap();

        // Delete + second push.
        lib.delete_folder(&folder.folder_id).unwrap();
        let plan = plan_push(&lib).unwrap();
        execute_push(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();

        assert!(
            !metadata_path.exists(),
            "Delete op must hard-remove <uuid>.metadata from device, not upload deleted:true",
        );
        assert!(
            !stray.exists(),
            "Delete op must sweep every <uuid>* artefact, not just .metadata",
        );
        // The library row is gone too (mark_folder_pushed drops tombstones).
        assert!(lib.list_folders().unwrap().is_empty());
    }

    #[tokio::test]
    async fn folder_delete_failure_keeps_push_pending() {
        // Regression guard for issue #23. When the device's
        // `delete_document_tree` fails (transient SFTP error,
        // permission denied, timeout), the folder push must remain
        // pending so a retry on the next sync can finish the
        // tombstone. Previously the SshDevice/FakeDevice impls
        // swallowed every per-file error and returned Ok, which
        // caused `mark_folder_pushed` to clear the queue and a
        // subsequent pull to resurrect the folder as a ghost.
        use async_trait::async_trait;
        use rehydrate_device::error::{DeviceError, DeviceResult};
        use rehydrate_device::model::{DeviceInfo, RemoteEntry, RemoteFile};
        use rehydrate_device::trait_def::Device;

        struct FailingDeleteDevice<D: Device>(D);

        #[async_trait]
        impl<D: Device> Device for FailingDeleteDevice<D> {
            async fn ping(&self) -> DeviceResult<DeviceInfo> {
                self.0.ping().await
            }
            async fn list_documents(&self) -> DeviceResult<Vec<RemoteEntry>> {
                self.0.list_documents().await
            }
            async fn fetch_document_tree(&self, uuid: &str) -> DeviceResult<Vec<RemoteFile>> {
                self.0.fetch_document_tree(uuid).await
            }
            async fn put_document_tree(
                &self,
                uuid: &str,
                files: &[RemoteFile],
            ) -> DeviceResult<()> {
                self.0.put_document_tree(uuid, files).await
            }
            async fn delete_document_tree(&self, _uuid: &str) -> DeviceResult<()> {
                Err(DeviceError::Other("simulated transient sftp error".into()))
            }
        }

        let lib_dir = tempfile::tempdir().unwrap();
        let dev_dir = tempfile::tempdir().unwrap();
        let lib = Library::open(lib_dir.path()).unwrap();
        let inner = FakeDevice::new(dev_dir.path());

        // First push (with the underlying fake) lands the metadata.
        let folder = lib.create_folder("Journal", None).unwrap();
        let plan = plan_push(&lib).unwrap();
        execute_push(&lib, &inner, plan, None, Cancel::default())
            .await
            .unwrap();
        assert_eq!(lib.list_pending_folder_pushes().unwrap().len(), 0);

        // Now wrap the device so the delete leg always fails, and
        // attempt a delete + push. The push must report a skipped
        // op (not a successful one) and the folder push must remain
        // queued so the next sync retries it.
        let dev = FailingDeleteDevice(inner);
        lib.delete_folder(&folder.folder_id).unwrap();
        let plan = plan_push(&lib).unwrap();
        let report = execute_push(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();
        assert!(
            report.skipped >= 1,
            "failed device delete must increment skipped, got report={report:?}",
        );

        let still_pending = lib.list_pending_folder_pushes().unwrap();
        assert!(
            still_pending
                .iter()
                .any(|op| op.folder_id() == folder.folder_id),
            "folder delete must remain pending after device failure, queue={still_pending:?}",
        );
    }

    #[tokio::test]
    async fn folder_reparent_pushes_new_parent_in_metadata_payload() {
        // Regression guard for commit 49e935b. The core test in
        // rehydrate-core proves the queue payload carries the new
        // parent. This one closes the loop by reading what landed on
        // the device — a future refactor that silently stripped or
        // re-keyed the `parent` field between queue and
        // put_document_tree would slip past the queue-side test.
        let lib_dir = tempfile::tempdir().unwrap();
        let dev_dir = tempfile::tempdir().unwrap();
        let lib = Library::open(lib_dir.path()).unwrap();
        let dev = FakeDevice::new(dev_dir.path());

        let parent = lib.create_folder("Journal", None).unwrap();
        let child = lib.create_folder("BH", None).unwrap();
        let plan = plan_push(&lib).unwrap();
        execute_push(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();

        // Drag BH under Journal.
        lib.reorder_folder(&child.folder_id, Some(&parent.folder_id), 0.0)
            .unwrap();
        let plan = plan_push(&lib).unwrap();
        execute_push(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();

        let metadata_path = dev_dir.path().join(format!("{}.metadata", child.folder_id));
        let bytes = std::fs::read(&metadata_path).expect("child metadata on device");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v.get("parent").and_then(|x| x.as_str()),
            Some(parent.folder_id.as_str()),
            "device-side metadata must carry the new parent uuid",
        );
    }

    /// Issue #17: cover archive soft-delete propagation. When the
    /// user archives a doc locally, `archive_document` writes a new
    /// version whose `.metadata` carries `deleted: true` and
    /// `parent: "trash"`. The next push must ship that metadata to
    /// the device — that's the channel by which xochitl learns to
    /// move the doc to its Trash view. A regression that classified
    /// archived rows as Skipped or stripped the mutation would
    /// silently strand the user's archive intent on the host while
    /// the device kept showing the doc as live.
    #[tokio::test]
    async fn archive_propagates_deleted_metadata_to_device_on_next_push() {
        let lib_dir = tempfile::tempdir().unwrap();
        let dev_dir = tempfile::tempdir().unwrap();
        let lib = Library::open(lib_dir.path()).unwrap();
        let dev = FakeDevice::new(dev_dir.path());

        // Seed a doc with a real (parseable JSON) `.metadata` blob,
        // since `archive_document` reads → mutates → re-writes it.
        // `b"{}"` is a legal empty JSON object — the archive mutation
        // just adds `deleted` and `parent` fields to it. We record
        // via `Source::Imported` (not the pre-faked-last_seen path
        // `seed_doc` uses) so the initial push actually uploads —
        // we need the device to start with the doc on disk to make
        // the post-archive "metadata is now deleted=true" assertion
        // meaningful.
        let mut m1 = Manifest::new("doc-1", "Notebook", "Doc One");
        let meta1 = lib.put_blob(b"{}").unwrap();
        let content1 = lib.put_blob(b"{}").unwrap();
        m1.files.push(ManifestFile {
            path: "doc-1.metadata".into(),
            sha256: meta1.hash,
            size: meta1.size,
            mode: 0o644,
            derived: false,
        });
        m1.files.push(ManifestFile {
            path: "doc-1.content".into(),
            sha256: content1.hash,
            size: content1.size,
            mode: 0o644,
            derived: false,
        });
        lib.record_version(&m1, Source::Imported).unwrap();
        execute_push(
            &lib,
            &dev,
            plan_push(&lib).unwrap(),
            None,
            Cancel::default(),
        )
        .await
        .unwrap();
        // Sanity: device received the initial (non-deleted) metadata.
        let pre = std::fs::read(dev_dir.path().join("doc-1.metadata")).unwrap();
        let pre_v: serde_json::Value = serde_json::from_slice(&pre).unwrap();
        assert_ne!(
            pre_v.get("deleted").and_then(|v| v.as_bool()),
            Some(true),
            "pre-archive device metadata must not be marked deleted",
        );

        // User archives locally.
        lib.archive_document("doc-1", rehydrate_core::ArchiveReason::Local)
            .unwrap();

        // plan_push must now classify the archived row as Outbound
        // — its `current_manifest` (the deleted=true revision) no
        // longer matches the device's last_seen_manifest.
        let plan = plan_push(&lib).unwrap();
        let outbound: Vec<_> = plan
            .items
            .iter()
            .filter(|i| i.status == PushItemStatus::Outbound)
            .collect();
        assert_eq!(
            outbound.len(),
            1,
            "archived doc must surface as Outbound, plan={plan:?}",
        );
        assert_eq!(outbound[0].document.document_id, "doc-1");
        let report = execute_push(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();
        assert_eq!(report.pushed, 1);

        // Device-side metadata now carries the archive mutation.
        let post = std::fs::read(dev_dir.path().join("doc-1.metadata")).unwrap();
        let post_v: serde_json::Value = serde_json::from_slice(&post).unwrap();
        assert_eq!(
            post_v.get("deleted").and_then(|v| v.as_bool()),
            Some(true),
            "post-archive device metadata must be deleted=true, got {post_v}",
        );
        assert_eq!(
            post_v.get("parent").and_then(|v| v.as_str()),
            Some("trash"),
            "archive must reparent to trash so xochitl moves it",
        );

        // Re-planning shows nothing outbound — last_seen advanced.
        let plan2 = plan_push(&lib).unwrap();
        assert!(
            plan2
                .items
                .iter()
                .all(|i| i.status != PushItemStatus::Outbound),
            "after a successful archive push, the row must no longer be outbound",
        );
    }

    /// Issue #17: cover version restore round-trip. The user picks
    /// an older version from the history drawer; `restore_version`
    /// re-records that older manifest as the new current. The push
    /// then ships the older content back to the device — exactly
    /// the "undo a device-side edit" affordance the version log
    /// exists for. A regression that left `current_manifest`
    /// unchanged after a restore would silently no-op the push and
    /// the user's intent (restore-and-replace on device) would be
    /// lost.
    #[tokio::test]
    async fn version_restore_pushes_the_restored_bytes_back_to_device() {
        let lib_dir = tempfile::tempdir().unwrap();
        let dev_dir = tempfile::tempdir().unwrap();
        let lib = Library::open(lib_dir.path()).unwrap();
        let dev = FakeDevice::new(dev_dir.path());

        // v1: original content. We record + push manually (instead
        // of via seed_doc) so `last_seen_manifest` stays NULL and
        // the first push actually uploads v1 to the device. Otherwise
        // every later assertion against the device file would be
        // reading the wrong revision.
        let mut m1 = Manifest::new("doc-1", "Notebook", "Doc One");
        let meta_v1 = lib.put_blob(b"{\"version\":1}").unwrap();
        let content_v1 = lib.put_blob(b"{\"v\":1}").unwrap();
        m1.files.push(ManifestFile {
            path: "doc-1.metadata".into(),
            sha256: meta_v1.hash,
            size: meta_v1.size,
            mode: 0o644,
            derived: false,
        });
        m1.files.push(ManifestFile {
            path: "doc-1.content".into(),
            sha256: content_v1.hash,
            size: content_v1.size,
            mode: 0o644,
            derived: false,
        });
        lib.record_version(&m1, Source::Imported).unwrap();
        execute_push(
            &lib,
            &dev,
            plan_push(&lib).unwrap(),
            None,
            Cancel::default(),
        )
        .await
        .unwrap();
        // History row id for v1 — what we'll restore back to.
        let history_after_v1 = lib.get_history("doc-1").unwrap();
        assert_eq!(history_after_v1.len(), 1);
        let v1_id = history_after_v1[0].id;

        // v2: simulate a later library-side edit (Source::Imported so
        // last_seen stays at v1's hash and the next plan_push surfaces
        // the v1→v2 delta as Outbound).
        let mut m2 = Manifest::new("doc-1", "Notebook", "Doc One");
        let meta_v2 = lib.put_blob(b"{\"version\":2}").unwrap();
        let content_v2 = lib.put_blob(b"{\"v\":2}").unwrap();
        m2.files.push(ManifestFile {
            path: "doc-1.metadata".into(),
            sha256: meta_v2.hash,
            size: meta_v2.size,
            mode: 0o644,
            derived: false,
        });
        m2.files.push(ManifestFile {
            path: "doc-1.content".into(),
            sha256: content_v2.hash,
            size: content_v2.size,
            mode: 0o644,
            derived: false,
        });
        lib.record_version(&m2, Source::Imported).unwrap();
        execute_push(
            &lib,
            &dev,
            plan_push(&lib).unwrap(),
            None,
            Cancel::default(),
        )
        .await
        .unwrap();
        // Sanity: device now has v2.
        let on_device_v2 = std::fs::read(dev_dir.path().join("doc-1.metadata")).unwrap();
        assert_eq!(on_device_v2, b"{\"version\":2}");

        // User clicks "Restore" on the v1 history row.
        let restore_outcome = lib.restore_version(v1_id).unwrap();
        assert!(
            !restore_outcome.unchanged,
            "restoring to an older version must produce a fresh manifest record",
        );

        // plan_push surfaces the restore as Outbound — last_seen is
        // pinned to v2's hash while current_manifest is now v1's.
        let plan = plan_push(&lib).unwrap();
        let outbound: Vec<_> = plan
            .items
            .iter()
            .filter(|i| i.status == PushItemStatus::Outbound)
            .collect();
        assert_eq!(
            outbound.len(),
            1,
            "restore_version must mark the doc Outbound, plan={plan:?}",
        );

        let report = execute_push(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();
        assert_eq!(report.pushed, 1);

        // The device should now hold v1's content again. That's
        // the restore round-trip the user expected: history rolls
        // back the tablet, not just the library view.
        let on_device_after_restore = std::fs::read(dev_dir.path().join("doc-1.metadata")).unwrap();
        assert_eq!(
            on_device_after_restore, b"{\"version\":1}",
            "device-side metadata must be v1's bytes after restore+push",
        );
        let on_device_content_after_restore =
            std::fs::read(dev_dir.path().join("doc-1.content")).unwrap();
        assert_eq!(on_device_content_after_restore, b"{\"v\":1}");
    }

    /// Issue #17: cover folder rename push. The existing
    /// `folder_reparent_pushes_new_parent_in_metadata_payload` test
    /// covers the *parent* field; the rename case (visibleName
    /// change) is a different mutation path through
    /// `rename_folder` → folder push queue → device. The device
    /// receives a fresh `<folder_id>.metadata` payload whose
    /// `visibleName` field reflects the new name. A regression
    /// that wired the rename to the wrong field or dropped the
    /// queue entry would surface here as the device keeping the
    /// pre-rename name.
    #[tokio::test]
    async fn folder_rename_pushes_new_visible_name_to_device() {
        let lib_dir = tempfile::tempdir().unwrap();
        let dev_dir = tempfile::tempdir().unwrap();
        let lib = Library::open(lib_dir.path()).unwrap();
        let dev = FakeDevice::new(dev_dir.path());

        let folder = lib.create_folder("Old name", None).unwrap();
        let plan = plan_push(&lib).unwrap();
        execute_push(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();

        // Device-side metadata reflects the initial create.
        let metadata_path = dev_dir
            .path()
            .join(format!("{}.metadata", folder.folder_id));
        let pre_bytes = std::fs::read(&metadata_path).expect("folder metadata on device");
        let pre_v: serde_json::Value = serde_json::from_slice(&pre_bytes).unwrap();
        assert_eq!(
            pre_v.get("visibleName").and_then(|x| x.as_str()),
            Some("Old name"),
        );

        // Rename. mark_folder_pushed cleared the previous Upsert,
        // so the rename enqueues a fresh one.
        lib.rename_folder(&folder.folder_id, "New name").unwrap();

        let plan = plan_push(&lib).unwrap();
        execute_push(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();

        // Device-side metadata now carries the new visibleName.
        let post_bytes = std::fs::read(&metadata_path).expect("folder metadata on device");
        let post_v: serde_json::Value = serde_json::from_slice(&post_bytes).unwrap();
        assert_eq!(
            post_v.get("visibleName").and_then(|x| x.as_str()),
            Some("New name"),
            "folder rename must reach the device via the next push",
        );

        // And the folder push queue should now be empty — a
        // regression that left a duplicate Upsert in the queue
        // would surface as the same metadata being re-pushed on
        // every subsequent sync.
        let pending = lib.list_pending_folder_pushes().unwrap();
        assert!(
            !pending.iter().any(|op| op.folder_id() == folder.folder_id),
            "folder push queue must be cleared after a successful rename push, got {pending:?}",
        );
    }

    #[tokio::test]
    async fn derived_files_are_not_pushed_to_device() {
        // Library-side artefacts (OCR transcripts, future caches)
        // travel with the manifest for versioning + restore but must
        // never end up on the tablet's xochitl file index.
        let lib_dir = tempfile::tempdir().unwrap();
        let dev_dir = tempfile::tempdir().unwrap();
        let lib = Library::open(lib_dir.path()).unwrap();
        let dev = FakeDevice::new(dev_dir.path());

        seed_doc(
            &lib,
            "doc-1",
            "Doc One",
            &[("doc-1.metadata", b"{}"), ("doc-1.content", b"{}")],
        );
        lib.record_derived_artefact("doc-1", "ocr/transcript.md", b"# Hi")
            .unwrap();

        let plan = plan_push(&lib).unwrap();
        execute_push(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();

        // The derived file must not be present on the device.
        let on_device = dev_dir.path().join("ocr").join("transcript.md");
        assert!(!on_device.exists(), "derived file leaked to device");
    }
}
