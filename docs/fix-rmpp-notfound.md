# Fix: RMPP "No such file" not classified as NotFound, causing documents to be skipped

## The bug

When syncing from an RMPP (reMarkable Paper Pro), documents that have no per-document
subdirectory (i.e. `<xochitl>/<uuid>/` does not exist on the device) are silently dropped
from the sync rather than treated as having no extra files.

The root cause is in `sftp_err()` in `crates/rehydrate-device/src/ssh.rs`.

The RM2 firmware's SFTP server surfaces "no such file or directory" errors as the string
`"NoSuchFile"` (the russh_sftp enum variant name). `sftp_err` checks for that and for
`"ENOENT"` and maps either to `DeviceError::NotFound`. That is correct for the RM2.

The RMPP firmware's SFTP server sends a different string: `"No such file"` — lowercase
first letter, with a space, no "or directory". That string matches neither check, so it
falls through to `DeviceError::Other`, which the caller treats as a genuine SFTP failure
and bails out of the whole document.

## There are two places to fix

### Fix 1 — `sftp_err()` (line 573)

```rust
// before
if msg.contains("NoSuchFile") || msg.contains("ENOENT") {

// after
if msg.contains("NoSuchFile") || msg.contains("No such file") || msg.contains("ENOENT") {
```

### Fix 2 — inline check at the per-document subdirectory read (around line 986)

The caller that reads `<xochitl>/<uuid>/` does not use the return value of `sftp_err()`;
it duplicates the string-matching logic directly:

```rust
// before
let msg = e.to_string();
if !msg.contains("NoSuchFile") && !msg.contains("ENOENT") {
    return Err(sftp_err("read_dir", &subdir, e));
}
```

This means fixing `sftp_err()` alone is not enough — this site will still misclassify the
RMPP error. Replace the inline string check with a call to `sftp_err()` and a variant match:

```rust
// after
let classified = sftp_err("read_dir", &subdir, e);
if !matches!(classified, DeviceError::NotFound(_)) {
    return Err(classified);
}
```

This also eliminates the duplication: the only place that knows what strings mean
"not found" is now `sftp_err()`.

## Why the document is skipped, not just the directory

The per-document subdirectory read is step 2 of a three-step fetch sequence (flat files →
per-uuid dir → thumbnails dir). When step 2 returns `DeviceError::Other`, the `?` operator
propagates it immediately and steps 3 and the enclosing per-document loop iteration are both
abandoned. The document ends up with zero blobs fetched and is left out of the library.

With `DeviceError::NotFound`, the `if !matches!` guard lets execution continue to step 3
and then to the next document, which is the correct behaviour: the per-uuid directory is
optional (notebooks have it; PDFs and EPUBs often do not).
