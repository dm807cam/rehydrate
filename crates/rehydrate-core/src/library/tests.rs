use super::*;
use crate::blob::PutOutcome;
use crate::manifest::{Manifest, ManifestFile};
use rusqlite::params;

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
        .import_file(&src, ImportKind::Pdf, "Sample document", None)
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
