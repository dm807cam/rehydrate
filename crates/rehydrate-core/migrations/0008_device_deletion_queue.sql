-- Tombstone queue for documents purged locally that still need to be
-- removed from the tablet.
--
-- The normal deletion flow is:
--   archive_document  → writes deleted=true manifest, marks for push.
--   sync push         → sends deleted=true metadata to xochitl (soft-delete).
--   purge_archived_document → hard-removes via SFTP.
--
-- If the user purges *before* syncing (or while the tablet is
-- unreachable), the immediate SFTP delete attempt in
-- `purge_archived_document` is skipped. Without this table the
-- document's sync_state row is wiped, so the next pull classifies the
-- still-present device document as New and re-downloads it — exactly
-- the opposite of what the user intended.
--
-- When `purge_archived_document` is called for a document that has a
-- non-NULL last_seen_manifest (i.e. was previously synced to the
-- tablet), the UUID is inserted here. The pull engine checks this
-- table before classifying each device entry: any UUID in the queue
-- is hard-deleted from the device instead of being downloaded, then
-- the row is removed.

CREATE TABLE device_deletion_queue (
    document_id TEXT PRIMARY KEY
);
