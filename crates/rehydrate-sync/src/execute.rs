use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use std::collections::HashSet;

use rehydrate_core::manifest::ManifestFile;
use rehydrate_core::{ArchiveReason, Library, Manifest, Source};
use rehydrate_device::Device;

use crate::error::{SyncError, SyncResult};
use crate::plan::{DocumentPlan, PlanItemStatus, PullPlan};
use crate::progress::{Progress, ProgressEvent};

/// Cancellation handle. The caller can flip this from any thread; the engine
/// checks between documents (the smallest unit of atomicity).
#[derive(Default, Clone)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    /// Clear the cancelled flag so the same shared handle can be
    /// reused for the next sync. Needed because the IPC layer keeps
    /// a single long-lived Cancel in AppState (so the renderer's
    /// "cancel" button can see it) rather than constructing a fresh
    /// one per call.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
pub struct SyncReport {
    pub recorded: usize,
    pub unchanged: usize,
    pub skipped: usize,
}

pub async fn execute_pull(
    library: &Library,
    device: &dyn Device,
    plan: PullPlan,
    progress: Option<Progress>,
    cancel: Cancel,
) -> SyncResult<SyncReport> {
    let total = plan.items.len();
    tracing::info!(total, "pull started");
    if let Some(p) = &progress {
        let _ = p
            .send(ProgressEvent::PlanReady {
                total_documents: total,
            })
            .await;
    }

    // Snapshot which document UUIDs the device just reported. Used after
    // the loop to detect device-side deletions (anything previously synced
    // and not present this time).
    let device_ids: HashSet<String> = plan
        .items
        .iter()
        .filter(|p| matches!(p.entry.kind, rehydrate_device::RemoteEntryKind::Document))
        .map(|p| p.entry.uuid.clone())
        .collect();

    let mut recorded = 0usize;
    let mut unchanged = 0usize;
    let mut skipped = 0usize;

    // Documents the user purged locally that still need to be removed from
    // the tablet. Loaded once so we don't query SQLite per device entry.
    let deletion_queue = library.list_device_deletion_queue().unwrap_or_default();

    for item in plan.items {
        if cancel.is_cancelled() {
            if let Some(p) = &progress {
                let _ = p.send(ProgressEvent::Cancelled).await;
            }
            return Err(SyncError::Cancelled);
        }

        // Folders are tracked but not "fetched"; Phase 1 stores them via
        // metadata only. Mirror device folders into the library db. A
        // single malformed folder shouldn't kill the whole pull, so
        // surface the error via the progress channel and keep going.
        if matches!(item.entry.kind, rehydrate_device::RemoteEntryKind::Folder) {
            if let Err(e) = mirror_folder(library, &item) {
                tracing::warn!(
                    folder = %item.entry.uuid,
                    error = %e,
                    "failed to mirror folder; continuing"
                );
                if let Some(p) = &progress {
                    let _ = p
                        .send(ProgressEvent::DocumentSkipped {
                            document_id: item.entry.uuid.clone(),
                            reason: format!("folder mirror failed: {e}"),
                        })
                        .await;
                }
            }
            continue;
        }

        // Documents the user purged locally while the tablet was unreachable.
        // Delete them from the device now rather than re-downloading them.
        if deletion_queue.contains(&item.entry.uuid) {
            match device.delete_document_tree(&item.entry.uuid).await {
                Ok(()) => {
                    tracing::info!(
                        uuid = %item.entry.uuid,
                        "deleted purged document from device"
                    );
                }
                Err(e) => {
                    // Not fatal — the queue entry stays and we retry next sync.
                    tracing::warn!(
                        uuid = %item.entry.uuid,
                        error = %e,
                        "failed to delete purged document from device (will retry)"
                    );
                }
            }
            // Dequeue: on success the file is gone; on "not found" errors it
            // was already absent. Only a transient error keeps retrying, but
            // leaving the entry doesn't break anything — next pull redoes this.
            let _ = library.dequeue_device_deletion(&item.entry.uuid);
            skipped += 1;
            continue;
        }

        if matches!(item.status, PlanItemStatus::Unchanged) {
            unchanged += 1;
            continue;
        }
        if matches!(item.status, PlanItemStatus::Skipped) {
            tracing::info!(
                uuid = %item.entry.uuid,
                name = %item.entry.visible_name,
                reason = item.reason.as_deref().unwrap_or(""),
                "pull: document skipped during planning"
            );
            skipped += 1;
            if let Some(p) = &progress {
                let _ = p
                    .send(ProgressEvent::DocumentSkipped {
                        document_id: item.entry.uuid.clone(),
                        reason: item.reason.clone().unwrap_or_default(),
                    })
                    .await;
            }
            continue;
        }

        if let Some(p) = &progress {
            let _ = p
                .send(ProgressEvent::DocumentStarted {
                    document_id: item.entry.uuid.clone(),
                    visible_name: item.entry.visible_name.clone(),
                })
                .await;
        }

        match fetch_and_record(library, device, &item, progress.as_ref()).await {
            Ok(was_unchanged) => {
                if !was_unchanged {
                    tracing::info!(
                        uuid = %item.entry.uuid,
                        name = %item.entry.visible_name,
                        "pulled document"
                    );
                }
                if let Some(p) = &progress {
                    let _ = p
                        .send(ProgressEvent::DocumentCompleted {
                            document_id: item.entry.uuid.clone(),
                            unchanged: was_unchanged,
                        })
                        .await;
                }
                if was_unchanged {
                    unchanged += 1;
                } else {
                    recorded += 1;
                }
            }
            Err(e) => {
                tracing::warn!(
                    uuid = %item.entry.uuid,
                    name = %item.entry.visible_name,
                    error = %e,
                    "pull: document skipped due to fetch/record error"
                );
                skipped += 1;
                if let Some(p) = &progress {
                    let _ = p
                        .send(ProgressEvent::DocumentSkipped {
                            document_id: item.entry.uuid.clone(),
                            reason: e.to_string(),
                        })
                        .await;
                }
            }
        }
    }

    // Device-side deletions: any document the library believed lived on
    // the device, that the device did not return in this listing, gets
    // moved to the archive. The user can restore from there. Keeps the
    // app from silently throwing away history when something disappears
    // from the tablet (intentionally or otherwise).
    //
    // Safety guard: SFTP `read_dir` is page-based, and we have no
    // contract that says a short page = end-of-stream. A *fully*
    // empty listing (zero documents) on a library that previously
    // had many is the realistic signature of a truncated read — a
    // server bug, a TCP hiccup, or auth that lost the
    // directory-handle. Refuse to sweep in that specific case so a
    // bad listing can't shred the library. ANY non-zero listing
    // proceeds normally; a real bulk-delete still archives cleanly.
    //
    // The earlier "<50%" heuristic was overly defensive — a user
    // who legitimately deletes 60% of their tablet library would
    // see the guard fire forever (the library state doesn't update
    // until the sweep runs, so the same ratio repeats on every
    // subsequent sync) and have no way to reflect those deletes
    // short of a manual archive. Narrowing to "zero results on a
    // previously-populated library" keeps the truncation defence
    // while letting every real-world delete pattern through.
    if !cancel.is_cancelled() {
        match library.previously_synced_ids() {
            Ok(known) => {
                let prior_count = known.len();
                let device_doc_count = device_ids.len();
                let looks_like_truncated_listing = device_doc_count == 0 && prior_count >= 4;
                if looks_like_truncated_listing {
                    tracing::warn!(
                        prior_count,
                        "device listing returned zero documents but library \
                         expected {prior_count}; skipping the device-deletion \
                         sweep to avoid mass-archiving on a truncated read. \
                         Re-sync to retry."
                    );
                } else {
                    for doc_id in known {
                        if device_ids.contains(&doc_id) {
                            continue;
                        }
                        tracing::info!(
                            uuid = %doc_id,
                            "archiving document no longer on device"
                        );
                        if let Err(e) = library.archive_document(&doc_id, ArchiveReason::Device) {
                            tracing::warn!(
                                uuid = %doc_id,
                                error = %e,
                                "could not archive device-deleted document"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not enumerate previously-synced ids");
            }
        }
    }

    if let Some(p) = &progress {
        let _ = p
            .send(ProgressEvent::Done {
                recorded,
                unchanged,
                skipped,
            })
            .await;
    }
    tracing::info!(recorded, unchanged, skipped, "pull complete");
    Ok(SyncReport {
        recorded,
        unchanged,
        skipped,
    })
}

fn mirror_folder(library: &Library, item: &DocumentPlan) -> SyncResult<()> {
    let metadata_json =
        serde_json::to_string(&item.entry.metadata).unwrap_or_else(|_| "null".into());
    library.upsert_folder(
        &item.entry.uuid,
        item.entry.parent.as_deref(),
        &item.entry.visible_name,
        &metadata_json,
    )?;
    Ok(())
}

async fn fetch_and_record(
    library: &Library,
    device: &dyn Device,
    item: &DocumentPlan,
    progress: Option<&Progress>,
) -> SyncResult<bool> {
    let files = device.fetch_document_tree(&item.entry.uuid).await?;

    // Hash + store every file. Build the manifest as we go.
    let mut manifest = Manifest::new(
        &item.entry.uuid,
        &item.entry.doc_type,
        &item.entry.visible_name,
    );
    manifest.parent = item.entry.parent.clone();
    manifest.metadata = item.entry.metadata.clone();

    // The `.content` file, if present, becomes content_meta.
    if let Some(content_file) = files
        .iter()
        .find(|f| f.path == format!("{}.content", item.entry.uuid))
    {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&content_file.bytes) {
            manifest.content_meta = v;
        }
    }

    for f in &files {
        let res = library.put_blob(&f.bytes)?;
        manifest.files.push(ManifestFile {
            path: f.path.clone(),
            sha256: res.hash.clone(),
            size: res.size,
            mode: f.mode,
            derived: false,
        });
        if let Some(p) = progress {
            let _ = p
                .send(ProgressEvent::FileFetched {
                    document_id: item.entry.uuid.clone(),
                    file: f.path.clone(),
                    bytes: res.size,
                    deduped: matches!(res.outcome, rehydrate_core::PutOutcome::Deduplicated),
                })
                .await;
        }
    }

    let outcome = library.record_version(&manifest, Source::Pulled)?;

    // Cache the device-side mtime hint for fast next-time classification.
    if let Some(hint) = &item.entry.device_mtime_hint {
        library.update_mtime_hint(&item.entry.uuid, hint)?;
    }

    Ok(outcome.unchanged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rehydrate_device::fake::FakeDevice;
    use std::fs;

    fn seed_fake_doc(root: &std::path::Path, uuid: &str, visible_name: &str, page_bytes: &[u8]) {
        fs::write(
            root.join(format!("{uuid}.metadata")),
            format!(
                r#"{{"visibleName":"{visible_name}","type":"DocumentType","lastModified":"100"}}"#
            ),
        )
        .unwrap();
        fs::write(
            root.join(format!("{uuid}.content")),
            r#"{"fileType":"notebook","pageCount":1}"#,
        )
        .unwrap();
        fs::create_dir_all(root.join(uuid)).unwrap();
        fs::write(root.join(uuid).join("page-1.rm"), page_bytes).unwrap();
    }

    #[tokio::test]
    async fn clean_pull_records_versions_and_dedupes() {
        let dev_root = tempfile::tempdir().unwrap();
        let lib_root = tempfile::tempdir().unwrap();
        seed_fake_doc(dev_root.path(), "doc-a", "A", b"shared");
        seed_fake_doc(dev_root.path(), "doc-b", "B", b"shared");

        let lib = rehydrate_core::Library::open(lib_root.path()).unwrap();
        let dev = FakeDevice::new(dev_root.path());

        let plan = crate::plan::plan_pull(&lib, &dev).await.unwrap();
        assert_eq!(plan.items.len(), 2);
        assert!(plan.items.iter().all(|p| p.status == PlanItemStatus::New));

        let report = execute_pull(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();
        assert_eq!(report.recorded, 2);
        assert_eq!(report.unchanged, 0);

        // Re-pull: now everything should be unchanged.
        let plan2 = crate::plan::plan_pull(&lib, &dev).await.unwrap();
        let report2 = execute_pull(&lib, &dev, plan2, None, Cancel::default())
            .await
            .unwrap();
        assert_eq!(report2.recorded, 0);
        assert_eq!(report2.unchanged, 2);

        // The shared "shared" page bytes should exist as a single blob.
        let lib_blobs = lib_root.path().join("blobs");
        let shared_hash = rehydrate_core::Sha256Hex::from_bytes(b"shared");
        let mut count = 0;
        for entry in walkdir::walk(&lib_blobs) {
            if entry.file_name().and_then(|n| n.to_str()) == Some(shared_hash.as_str()) {
                count += 1;
            }
        }
        assert_eq!(count, 1, "shared page must be deduplicated");
    }

    /// Issue #17: cover "updated doc" — the device-side metadata
    /// changes between two pulls and the library must record a new
    /// version. Pre-existing `clean_pull_records_versions_and_dedupes`
    /// only covers New + Unchanged, leaving the Changed→record path
    /// (the most common steady-state pull shape) untested.
    #[tokio::test]
    async fn pull_records_a_new_version_when_device_metadata_changes() {
        let dev_root = tempfile::tempdir().unwrap();
        let lib_root = tempfile::tempdir().unwrap();
        // Initial pull: visibleName = "Original" with mtime hint 100.
        seed_fake_doc(dev_root.path(), "doc-x", "Original", b"page-v1");
        let lib = rehydrate_core::Library::open(lib_root.path()).unwrap();
        let dev = FakeDevice::new(dev_root.path());

        let plan = crate::plan::plan_pull(&lib, &dev).await.unwrap();
        execute_pull(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();
        let history_before = lib.get_history("doc-x").unwrap();
        assert_eq!(history_before.len(), 1, "first pull records v1");

        // Device-side edit: rename + bump mtime so plan_pull
        // classifies as Changed (not Unchanged).
        fs::write(
            dev_root.path().join("doc-x.metadata"),
            r#"{"visibleName":"Renamed","type":"DocumentType","lastModified":"200"}"#,
        )
        .unwrap();

        let plan2 = crate::plan::plan_pull(&lib, &dev).await.unwrap();
        assert_eq!(plan2.items.len(), 1);
        assert!(
            matches!(plan2.items[0].status, PlanItemStatus::Changed),
            "device-side mtime bump must surface as Changed, got {:?}",
            plan2.items[0].status,
        );
        let report = execute_pull(&lib, &dev, plan2, None, Cancel::default())
            .await
            .unwrap();
        assert_eq!(report.recorded, 1, "edited doc must be recorded");

        // The history must now have TWO versions, chained by
        // parent_version_id. Without this assertion a future
        // regression that overwrote in place (instead of appending)
        // would silently lose the v1 snapshot the user could revert
        // to.
        let history_after = lib.get_history("doc-x").unwrap();
        assert_eq!(history_after.len(), 2, "edited doc must accrue a v2");
        assert_eq!(
            history_after[1].parent_version_id,
            Some(history_after[0].id),
            "v2 must chain back to v1's id",
        );

        // The library's current view should show the renamed doc.
        let live = lib.list_documents().unwrap();
        let entry = live.iter().find(|d| d.document_id == "doc-x").unwrap();
        assert_eq!(entry.visible_name, "Renamed");
    }

    /// Issue #17: cover "deleted-on-tablet doc" — the device-deletion
    /// sweep at the bottom of execute_pull moves docs the device no
    /// longer reports into archived_documents with
    /// ArchiveReason::Device. A regression here would silently lose
    /// the user's device-side delete intent (they removed it on the
    /// tablet but the library keeps showing it as live).
    #[tokio::test]
    async fn pull_archives_a_doc_the_device_no_longer_reports() {
        let dev_root = tempfile::tempdir().unwrap();
        let lib_root = tempfile::tempdir().unwrap();
        seed_fake_doc(dev_root.path(), "doc-a", "Will Survive", b"a");
        seed_fake_doc(dev_root.path(), "doc-b", "Will Vanish", b"b");
        let lib = rehydrate_core::Library::open(lib_root.path()).unwrap();
        let dev = FakeDevice::new(dev_root.path());

        // Initial pull picks up both.
        let plan = crate::plan::plan_pull(&lib, &dev).await.unwrap();
        execute_pull(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();
        assert_eq!(lib.list_documents().unwrap().len(), 2);

        // Remove doc-b from the device — every sidecar and its
        // per-uuid directory. Mirrors xochitl's actual on-device
        // delete shape.
        let _ = fs::remove_file(dev_root.path().join("doc-b.metadata"));
        let _ = fs::remove_file(dev_root.path().join("doc-b.content"));
        let _ = fs::remove_dir_all(dev_root.path().join("doc-b"));

        // Pull again. plan_pull sees only doc-a; execute's sweep at
        // the bottom of the function archives doc-b.
        let plan2 = crate::plan::plan_pull(&lib, &dev).await.unwrap();
        assert_eq!(plan2.items.len(), 1, "only doc-a is listed");
        execute_pull(&lib, &dev, plan2, None, Cancel::default())
            .await
            .unwrap();

        // Post-state: doc-a is still live, doc-b is in archived.
        let live: Vec<_> = lib
            .list_documents()
            .unwrap()
            .into_iter()
            .map(|d| d.document_id)
            .collect();
        assert_eq!(live, vec!["doc-a".to_string()]);
        assert!(lib.is_archived("doc-b").unwrap());
        let archived = lib.list_archived().unwrap();
        let b = archived.iter().find(|a| a.document_id == "doc-b").unwrap();
        assert!(
            matches!(b.reason, rehydrate_core::ArchiveReason::Device),
            "device-side delete must surface as ArchiveReason::Device, got {:?}",
            b.reason,
        );
    }

    /// Issue #17: cover the truncated-listing safety guard. An SFTP
    /// server bug, a TCP hiccup, or an auth path that lost the
    /// directory handle can return an empty NAME list on a library
    /// that previously had many docs. Without the guard the
    /// device-deletion sweep would mass-archive every doc the user
    /// has — a single bad listing would shred the library. The
    /// engine refuses to sweep when the device returns zero and the
    /// library has ≥ 4 previously-synced docs.
    #[tokio::test]
    async fn pull_truncated_listing_does_not_mass_archive_a_populated_library() {
        let dev_root = tempfile::tempdir().unwrap();
        let lib_root = tempfile::tempdir().unwrap();
        // Seed at least 4 docs so we trip the >=4-prior-count guard.
        for i in 0..4 {
            seed_fake_doc(
                dev_root.path(),
                &format!("doc-{i}"),
                &format!("Doc {i}"),
                format!("page-{i}").as_bytes(),
            );
        }
        let lib = rehydrate_core::Library::open(lib_root.path()).unwrap();
        let dev = FakeDevice::new(dev_root.path());

        let plan = crate::plan::plan_pull(&lib, &dev).await.unwrap();
        execute_pull(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();
        assert_eq!(lib.list_documents().unwrap().len(), 4);

        // Wipe every doc from the device — simulates a truncated
        // listing (we asked the user to pretend their SFTP server
        // momentarily returned an empty NAME list). The plan sees
        // zero docs.
        for i in 0..4 {
            let _ = fs::remove_file(dev_root.path().join(format!("doc-{i}.metadata")));
            let _ = fs::remove_file(dev_root.path().join(format!("doc-{i}.content")));
            let _ = fs::remove_dir_all(dev_root.path().join(format!("doc-{i}")));
        }
        let plan2 = crate::plan::plan_pull(&lib, &dev).await.unwrap();
        assert!(
            plan2.items.is_empty(),
            "empty device listing should produce an empty plan",
        );
        execute_pull(&lib, &dev, plan2, None, Cancel::default())
            .await
            .unwrap();

        // Critical assertion: the library MUST still report all 4
        // docs as live. A regression that removed the truncation
        // guard would surface here as zero live + all archived.
        assert_eq!(
            lib.list_documents().unwrap().len(),
            4,
            "truncated listing on a populated library must NOT trigger the device-deletion sweep",
        );
        assert!(
            lib.list_archived().unwrap().is_empty(),
            "no doc should have been archived from the truncated listing",
        );
    }

    /// Issue #17: cover the local-archive + device-still-has-it
    /// case. The user archived a doc locally; the device still
    /// reports it. `plan_pull` must classify the entry as Skipped
    /// with a clear reason rather than queue it for transfer (which
    /// would re-resurrect via `record_version` — which itself now
    /// rejects via the issue #31 DocumentArchived gate, so the
    /// resurrection would convert to a skipped count, but the
    /// plan-level skip is the cleaner contract). A regression
    /// removing the `is_archived` check in plan_pull would surface
    /// here as Changed/New instead of Skipped.
    #[tokio::test]
    async fn plan_pull_skips_locally_archived_docs() {
        let dev_root = tempfile::tempdir().unwrap();
        let lib_root = tempfile::tempdir().unwrap();
        seed_fake_doc(dev_root.path(), "doc-1", "Doc One", b"page");
        let lib = rehydrate_core::Library::open(lib_root.path()).unwrap();
        let dev = FakeDevice::new(dev_root.path());

        // Initial pull → archive locally.
        let plan = crate::plan::plan_pull(&lib, &dev).await.unwrap();
        execute_pull(&lib, &dev, plan, None, Cancel::default())
            .await
            .unwrap();
        lib.archive_document("doc-1", rehydrate_core::ArchiveReason::Local)
            .unwrap();
        assert!(lib.is_archived("doc-1").unwrap());

        // Pull again — plan_pull must mark the doc Skipped.
        let plan2 = crate::plan::plan_pull(&lib, &dev).await.unwrap();
        assert_eq!(plan2.items.len(), 1);
        let item = &plan2.items[0];
        assert_eq!(item.entry.uuid, "doc-1");
        assert!(
            matches!(item.status, PlanItemStatus::Skipped),
            "locally-archived doc must be Skipped, got {:?}",
            item.status,
        );
        assert!(
            item.reason.as_deref() == Some("archived locally"),
            "skip reason must be informative, got {:?}",
            item.reason,
        );

        // And executing the plan must NOT remove the archive entry.
        execute_pull(&lib, &dev, plan2, None, Cancel::default())
            .await
            .unwrap();
        assert!(
            lib.is_archived("doc-1").unwrap(),
            "archive entry must survive the pull",
        );
        assert!(
            lib.list_documents()
                .unwrap()
                .iter()
                .all(|d| d.document_id != "doc-1"),
            "archived doc must NOT appear in list_documents after the pull",
        );
    }

    /// Tiny dir walker used only by tests so we don't need a separate dev-dep.
    mod walkdir {
        use std::path::{Path, PathBuf};
        pub fn walk(p: &Path) -> Vec<PathBuf> {
            let mut out = Vec::new();
            if !p.exists() {
                return out;
            }
            for e in std::fs::read_dir(p).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    out.extend(walk(&path));
                } else {
                    out.push(path);
                }
            }
            out
        }
    }
}
