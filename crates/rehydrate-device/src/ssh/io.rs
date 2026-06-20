//! SFTP helpers shared by every ssh-module sibling: timeout wrappers,
//! tmp/bak path naming, the open_sftp dance, the error mapper, and
//! the size-capped reader. Lives outside `mod.rs` so the connection
//! machinery in `mod.rs` and the trait impl in `device_impl.rs` both
//! pull from a single source of truth.

use std::time::Duration;

use russh::client::Handle;
use russh_sftp::client::SftpSession;
use tokio::io::AsyncReadExt;

use crate::error::{DeviceError, DeviceResult};

use super::handler::ClientHandler;

/// Hard cap on a single SFTP read. Real reMarkable documents top out at a
/// few hundred megabytes for very large PDFs; anything past this is either
/// an exotic edge case the user needs to resolve manually, or a malicious /
/// corrupted device serving an oversized file. Without this cap the client
/// would happily allocate gigabytes from a single bad read.
const MAX_REMOTE_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// Bound on directory recursion depth inside `fetch_subtree[_named]`.
/// reMarkable's xochitl tree is essentially flat (one level under the
/// document UUID); 16 leaves enormous headroom while killing any pathological
/// loop that slipped past the symlink filter.
pub(super) const MAX_SUBTREE_DEPTH: usize = 16;

/// Per-operation SFTP timeout for "should be quick" calls — opens,
/// stats, directory listings, single chunk writes. 120s is generous;
/// these usually complete in milliseconds and only stall when the
/// SFTP server itself wedges, in which case we'd rather report a
/// typed `Timeout` than hold the device-wide mutex indefinitely.
const SFTP_OP_TIMEOUT: Duration = Duration::from_secs(120);

/// Per-operation SFTP timeout for full body reads. The reMarkable's
/// USB-ethernet SFTP throughput is typically 2–4 MB/s in practice
/// (the previous "5 MB/s" doc string was optimistic — user reports
/// settle nearer 3 MB/s on real devices). At the lower end, a 512 MB
/// notebook PDF needs ~3 minutes to transfer cleanly. 10 minutes
/// leaves comfortable headroom for a 500 MB file at half the typical
/// rate while still bounding the worst case so a wedged read can't
/// hang the app forever. We deliberately do NOT use `SFTP_OP_TIMEOUT`
/// for body reads — that 120s cap clipped legitimate large-file
/// pulls mid-transfer and was an audit regression.
const SFTP_BODY_READ_TIMEOUT: Duration = Duration::from_secs(600);

/// Wrap an SFTP-shaped future in a per-operation timeout. Used by
/// every `inner.sftp.*` open/stat/read_dir call so a single
/// misbehaving call can never hold the device-wide mutex past the
/// budget. Use [`with_body_timeout`] for the full-file body read
/// because that one is naturally long-running on big PDFs.
pub(super) async fn with_timeout<T, F>(label: &str, fut: F) -> DeviceResult<T>
where
    F: std::future::Future<Output = DeviceResult<T>>,
{
    match tokio::time::timeout(SFTP_OP_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(DeviceError::Timeout(label.to_string())),
    }
}

/// As [`with_timeout`] but with [`SFTP_BODY_READ_TIMEOUT`] — the
/// longer budget appropriate for "stream the whole file" operations.
pub(super) async fn with_body_timeout<T, F>(label: &str, fut: F) -> DeviceResult<T>
where
    F: std::future::Future<Output = DeviceResult<T>>,
{
    match tokio::time::timeout(SFTP_BODY_READ_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(DeviceError::Timeout(label.to_string())),
    }
}

pub(super) fn staged_path(path: &str) -> String {
    format!("{path}.rehydrate-tmp")
}

pub(super) fn backup_path(path: &str) -> String {
    format!("{path}.rehydrate-bak")
}

pub(super) async fn open_sftp(handle: &Handle<ClientHandler>) -> DeviceResult<SftpSession> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| DeviceError::Other(format!("channel_open_session: {e}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| DeviceError::Other(format!("request_subsystem(sftp): {e}")))?;
    let stream = channel.into_stream();
    SftpSession::new(stream)
        .await
        .map_err(|e| DeviceError::Other(format!("SftpSession::new: {e}")))
}

pub(super) fn sftp_err(op: &str, path: &str, e: russh_sftp::client::error::Error) -> DeviceError {
    // Prefer the typed status from russh-sftp so we don't depend on the
    // wording of the rendered error string. The reMarkable Paper Pro's
    // SFTP server returns Display "No such file" (with a space, mixed
    // case) — the older substring check looked for "NoSuchFile" (the
    // Debug spelling) and missed it, demoting a missing optional
    // directory to DeviceError::Other and skipping the whole document.
    use russh_sftp::client::error::Error as SftpError;
    use russh_sftp::protocol::StatusCode;
    if let SftpError::Status(s) = &e {
        if s.status_code == StatusCode::NoSuchFile {
            return DeviceError::NotFound(path.to_string());
        }
    }
    let msg = format!("{op} {path}: {e}");
    let lower = msg.to_ascii_lowercase();
    if lower.contains("no such file") || lower.contains("nosuchfile") || lower.contains("enoent") {
        DeviceError::NotFound(path.to_string())
    } else {
        DeviceError::Other(msg)
    }
}

pub(super) async fn read_path(sftp: &SftpSession, path: &str) -> DeviceResult<Vec<u8>> {
    let f = with_timeout("read_path.open", async {
        sftp.open(path).await.map_err(|e| sftp_err("open", path, e))
    })
    .await?;
    with_body_timeout("read_path.body", read_capped(f, path)).await
}

/// Read an SFTP file with a hard size limit. `take(MAX_REMOTE_FILE_BYTES + 1)`
/// lets us distinguish "file fits within budget" from "file exceeds budget"
/// without ever allocating beyond the cap.
pub(super) async fn read_capped<R>(reader: R, path: &str) -> DeviceResult<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut limited = reader.take(MAX_REMOTE_FILE_BYTES + 1);
    limited
        .read_to_end(&mut buf)
        .await
        .map_err(|e| DeviceError::Io(std::io::Error::other(e.to_string())))?;
    if buf.len() as u64 > MAX_REMOTE_FILE_BYTES {
        return Err(DeviceError::Other(format!(
            "remote file {path} exceeds {} byte cap",
            MAX_REMOTE_FILE_BYTES
        )));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::client::error::Error as SftpError;
    use russh_sftp::protocol::{Status, StatusCode};

    fn status_err(code: StatusCode, message: &str) -> SftpError {
        SftpError::Status(Status {
            id: 1,
            status_code: code,
            error_message: message.to_string(),
            language_tag: String::new(),
        })
    }

    // The reMarkable Paper Pro returns Status(NoSuchFile, "No such file")
    // — without the typed match this used to be classified as Other
    // and skip the whole document.
    #[test]
    fn typed_no_such_file_status_maps_to_not_found() {
        let e = status_err(StatusCode::NoSuchFile, "No such file");
        assert!(matches!(
            sftp_err("read_dir", "/some/path", e),
            DeviceError::NotFound(_)
        ));
    }

    #[test]
    fn legacy_camelcase_no_such_file_string_maps_to_not_found() {
        // Construct an IO-wrapped error whose Display includes the
        // older "NoSuchFile" spelling; the fallback substring match
        // (lowercased) should still classify it as NotFound.
        let e = SftpError::IO("NoSuchFile: somewhere".into());
        assert!(matches!(
            sftp_err("read_dir", "/some/path", e),
            DeviceError::NotFound(_)
        ));
    }

    #[test]
    fn enoent_in_message_maps_to_not_found() {
        let e = SftpError::IO("got ENOENT from kernel".into());
        assert!(matches!(
            sftp_err("read_dir", "/some/path", e),
            DeviceError::NotFound(_)
        ));
    }

    #[test]
    fn permission_denied_status_is_not_classified_as_not_found() {
        let e = status_err(StatusCode::PermissionDenied, "Permission denied");
        assert!(matches!(
            sftp_err("read_dir", "/some/path", e),
            DeviceError::Other(_)
        ));
    }

    #[test]
    fn unrelated_io_error_maps_to_other() {
        let e = SftpError::IO("connection reset".into());
        assert!(matches!(
            sftp_err("read_dir", "/some/path", e),
            DeviceError::Other(_)
        ));
    }
}
