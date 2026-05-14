//! SSH/SFTP transport against the reMarkable USB-ethernet endpoint at
//! `10.11.99.1:22`.
//!
//! Authentication uses the device password the user captures from the tablet
//! UI on first run (Settings → Help → Copyrights and licenses → "GPLv3
//! Compliance" reveals the SSH password). The password is held in memory in
//! a `secrecy::SecretString` and persisted to the OS keychain by the app
//! layer; this module never touches the keychain itself.
//!
//! Phase 1 needs only enumeration and download. The SSH session is held
//! across calls (long-lived TCP connection) and operations are serialised
//! through a `Mutex`. That's plenty fast over USB-ethernet — the bottleneck
//! is the device, not concurrency.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use russh::client::{self, Handle, Handler};
use russh::keys::PublicKey;
use russh::ChannelMsg;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::error::{DeviceError, DeviceResult};
use crate::known_hosts::KnownHosts;
use crate::model::{DeviceInfo, RemoteEntry, RemoteEntryKind, RemoteFile};
use crate::trait_def::Device;

pub const DEFAULT_HOST: &str = "10.11.99.1";
pub const DEFAULT_PORT: u16 = 22;
pub const DEFAULT_USER: &str = "root";
pub const XOCHITL_DIR: &str = "/home/root/.local/share/remarkable/xochitl";

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
const MAX_SUBTREE_DEPTH: usize = 16;

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
async fn with_timeout<T, F>(label: &str, fut: F) -> DeviceResult<T>
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
async fn with_body_timeout<T, F>(label: &str, fut: F) -> DeviceResult<T>
where
    F: std::future::Future<Output = DeviceResult<T>>,
{
    match tokio::time::timeout(SFTP_BODY_READ_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(DeviceError::Timeout(label.to_string())),
    }
}

#[derive(Debug, Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub xochitl_dir: String,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            user: DEFAULT_USER.to_string(),
            xochitl_dir: XOCHITL_DIR.to_string(),
        }
    }
}

/// russh client handler doing trust-on-first-use host-key pinning.
///
/// On first connect to a given `host:port` the server's SHA256
/// fingerprint is pinned. Every subsequent connect compares the
/// presented key to the pinned value and refuses to proceed when they
/// don't match — propagating the mismatch back to `SshDevice::connect`
/// through the shared `outcome` slot, which is then surfaced as
/// [`DeviceError::HostKeyChanged`].
///
/// The outcome is held in `Arc<std::sync::Mutex<_>>` rather than
/// directly on the handler because russh's `client::connect` consumes
/// the handler by value; sharing the slot lets the caller read the
/// verification result after the handshake completes.
struct ClientHandler {
    known_hosts: KnownHosts,
    endpoint: String,
    outcome: Arc<std::sync::Mutex<HostKeyOutcome>>,
}

#[derive(Debug, Clone, Default)]
enum HostKeyOutcome {
    /// Handshake hasn't happened yet (or was rejected outright by
    /// russh before our `check_server_key` ran).
    #[default]
    Pending,
    /// First time we've seen this host; the fingerprint will be pinned
    /// once the session is fully authenticated.
    PendingRecord { fingerprint: String },
    /// Pinned fingerprint matched the presented key. No action needed.
    Matched,
    /// Pinned fingerprint did not match — refuse.
    Changed { pinned: String, presented: String },
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // ssh-key's `Fingerprint` Display impl is the exact OpenSSH
        // shape `SHA256:<base64>` — exactly what `ssh-keygen -lf`
        // emits and what users will paste into known-hosts diagnostics.
        let presented = server_public_key
            .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
            .to_string();
        let next = match self.known_hosts.lookup(&self.endpoint) {
            Ok(Some(pinned)) if pinned == presented => HostKeyOutcome::Matched,
            Ok(Some(pinned)) => HostKeyOutcome::Changed {
                pinned,
                presented: presented.clone(),
            },
            Ok(None) => HostKeyOutcome::PendingRecord {
                fingerprint: presented.clone(),
            },
            Err(e) => {
                // Reading known_hosts shouldn't fail — but if it does,
                // refuse rather than fall back to "accept anything".
                tracing::error!("known_hosts read failed: {e}");
                return Err(russh::Error::IO(std::io::Error::other(e.to_string())));
            }
        };
        let accept = !matches!(next, HostKeyOutcome::Changed { .. });
        if let Ok(mut guard) = self.outcome.lock() {
            *guard = next;
        }
        Ok(accept)
    }
}

pub struct SshDevice {
    cfg: SshConfig,
    inner: Mutex<Inner>,
    /// One-shot warning collected at connect time. Set when the
    /// TOFU layer accepted the handshake (first-seen host) but the
    /// `known_hosts` write failed afterwards — typically a
    /// read-only / sandboxed config dir. The connection succeeds
    /// either way, but the next reconnect won't have a pinned
    /// fingerprint to compare against, so the security guarantee
    /// the audit added is silently absent. The app layer reads
    /// this via [`Self::take_pending_warning`] and surfaces it as
    /// a Tauri `host-key:warning` event so the user can act on it.
    pending_warning: std::sync::Mutex<Option<String>>,
}

struct Inner {
    handle: Handle<ClientHandler>,
    sftp: SftpSession,
}

impl SshDevice {
    /// Open an SSH session and SFTP subsystem against the configured host.
    ///
    /// The host's public key is pinned via TOFU using the supplied
    /// `known_hosts` store. A mismatched pinned fingerprint produces
    /// `DeviceError::HostKeyChanged` — see `known_hosts.rs` for the
    /// rationale and the file shape.
    pub async fn connect(
        cfg: SshConfig,
        password: SecretString,
        known_hosts: KnownHosts,
    ) -> DeviceResult<Self> {
        let russh_cfg = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(60)),
            keepalive_interval: Some(Duration::from_secs(15)),
            ..Default::default()
        });

        let endpoint = format!("{}:{}", cfg.host, cfg.port);
        let outcome: Arc<std::sync::Mutex<HostKeyOutcome>> = Arc::default();
        let handler = ClientHandler {
            known_hosts: known_hosts.clone(),
            endpoint: endpoint.clone(),
            outcome: Arc::clone(&outcome),
        };

        let connect_result =
            client::connect(russh_cfg, (cfg.host.as_str(), cfg.port), handler).await;

        // Translate a host-key mismatch into the typed error before
        // checking the connect Result — a mismatch causes
        // `check_server_key` to return Ok(false), which surfaces as
        // a generic russh disconnect rather than a useful message.
        if let Ok(guard) = outcome.lock() {
            if let HostKeyOutcome::Changed { pinned, presented } = &*guard {
                return Err(DeviceError::HostKeyChanged {
                    endpoint,
                    pinned_fingerprint: pinned.clone(),
                    presented_fingerprint: presented.clone(),
                });
            }
        }

        let mut handle = connect_result
            .map_err(|e| DeviceError::Unreachable(format!("{}:{} ({e})", cfg.host, cfg.port)))?;

        let auth_ok = handle
            .authenticate_password(&cfg.user, password.expose_secret())
            .await
            .map_err(|e| DeviceError::Other(format!("authenticate_password: {e}")))?;
        if !auth_ok.success() {
            return Err(DeviceError::AuthFailed);
        }

        // Auth succeeded — now is the right time to commit a
        // first-seen fingerprint. Doing this before auth would let an
        // impostor that handshakes successfully but rejects the
        // password silently overwrite the pinned key. Doing it after
        // means a successful login is also a confirmation that this
        // is the device the user expected.
        let mut pending_warning: Option<String> = None;
        if let Ok(guard) = outcome.lock() {
            if let HostKeyOutcome::PendingRecord { fingerprint } = &*guard {
                if let Err(e) = known_hosts.record(&endpoint, fingerprint) {
                    tracing::warn!("could not persist pinned host key: {e}");
                    // Surface to the UI: a record() failure means
                    // we accepted this key but won't recognise the
                    // device on reconnect, so the TOFU defence is
                    // disabled until the user fixes the underlying
                    // FS / permission issue.
                    pending_warning = Some(format!(
                        "Couldn't pin the tablet's host key — \
                         host-key change detection is OFF until this is fixed. \
                         Reason: {e}"
                    ));
                }
            }
        }

        let sftp = open_sftp(&handle).await?;

        let device = Self {
            cfg,
            inner: Mutex::new(Inner { handle, sftp }),
            pending_warning: std::sync::Mutex::new(pending_warning),
        };
        // Surface stranded `.rehydrate-bak` files in the xochitl
        // directory — these mean a previous push hit a double-fault
        // and the live file may have been lost. Logged at warn so the
        // user notices; actual recovery is performed by the next push
        // to the affected path (see `commit_staged`).
        device.scan_stranded_backups().await;
        Ok(device)
    }

    /// Best-effort scan for stranded `.rehydrate-bak` files in the
    /// xochitl directory. A leftover backup with no live counterpart
    /// signals a previous push crashed during the rename dance and
    /// the user's data is sitting at `<path>.rehydrate-bak`. The
    /// next `commit_staged` for the same path performs the actual
    /// recovery; this scan only surfaces the situation early so it
    /// doesn't go unnoticed until the user pushes that doc again.
    async fn scan_stranded_backups(&self) {
        let inner = self.inner.lock().await;
        let dir = self.cfg.xochitl_dir.clone();
        let entries = match with_timeout("scan_stranded.read_dir", async {
            inner
                .sftp
                .read_dir(&dir)
                .await
                .map_err(|e| sftp_err("read_dir", &dir, e))
        })
        .await
        {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("could not scan for stranded backups under {dir}: {e}");
                return;
            }
        };
        let mut stranded = Vec::new();
        for entry in entries {
            let name = entry.file_name();
            let Some(live_name) = name.strip_suffix(".rehydrate-bak") else {
                continue;
            };
            let live = format!("{dir}/{live_name}");
            let exists = inner.sftp.metadata(&live).await.is_ok();
            if !exists {
                stranded.push(format!("{dir}/{name}"));
            }
        }
        if !stranded.is_empty() {
            tracing::warn!(
                "found {} stranded .rehydrate-bak file(s) on device — \
                 the next push to the same path will recover them: {:?}",
                stranded.len(),
                stranded
            );
        }
    }

    pub fn config(&self) -> &SshConfig {
        &self.cfg
    }

    /// Consume and return the at-most-one warning collected at
    /// connect time. Currently fires when [`KnownHosts::record`]
    /// failed after a successful first-seen handshake, which means
    /// TOFU detection is OFF until the user fixes the underlying
    /// FS / permissions issue. App layer surfaces the message as a
    /// `host-key:warning` Tauri event. Returns `None` on a normal
    /// happy-path connect.
    ///
    /// "Take" rather than "peek" because the warning is a one-shot
    /// event tied to the connect call; subsequent reconnects
    /// produce their own state.
    pub fn take_pending_warning(&self) -> Option<String> {
        self.pending_warning.lock().ok().and_then(|mut g| g.take())
    }

    /// Run `cmd` and capture stdout. Used for the device-info probe; SFTP is
    /// preferred for everything else.
    async fn exec(&self, cmd: &str) -> DeviceResult<String> {
        let inner = self.inner.lock().await;
        let mut channel = inner
            .handle
            .channel_open_session()
            .await
            .map_err(|e| DeviceError::Other(format!("channel_open_session: {e}")))?;
        channel
            .exec(true, cmd)
            .await
            .map_err(|e| DeviceError::Other(format!("exec: {e}")))?;

        let mut out = Vec::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { ref data } => out.extend_from_slice(data),
                ChannelMsg::ExtendedData { .. } => {}
                ChannelMsg::ExitStatus { .. } => {}
                ChannelMsg::Eof => break,
                _ => {}
            }
        }
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    async fn read_file(&self, sftp: &SftpSession, path: &str) -> DeviceResult<Vec<u8>> {
        // Every SFTP touchpoint goes through `with_timeout` — without
        // it a stuck server-side read would hold the device-wide mutex
        // until the session-level inactivity_timeout fires (60 s) and
        // the entire SSH connection tears down.
        let f = with_timeout("read_file.open", async {
            sftp.open(path).await.map_err(|e| sftp_err("open", path, e))
        })
        .await?;
        with_body_timeout("read_file.body", read_capped(f, path)).await
    }

    /// Stage a file's bytes to `<path>.rehydrate-tmp`. Does NOT touch the
    /// live `<path>` — that's done in a second phase by `commit_staged`,
    /// after every file in the document has been staged. This split means
    /// a network interruption or write error mid-upload leaves the live
    /// document untouched on the device, instead of half-overwritten.
    async fn stage_file(&self, sftp: &SftpSession, path: &str, bytes: &[u8]) -> DeviceResult<()> {
        if let Some(parent) = path.rsplit_once('/').map(|(p, _)| p) {
            if !parent.is_empty() {
                let _ = sftp.create_dir(parent).await;
            }
        }
        let tmp_path = staged_path(path);
        let _ = sftp.remove_file(&tmp_path).await;

        let flags = OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE;
        let mut file = with_timeout("stage_file.open", async {
            sftp.open_with_flags(&tmp_path, flags)
                .await
                .map_err(|e| sftp_err("open_with_flags", &tmp_path, e))
        })
        .await?;
        with_timeout("stage_file.write", async {
            file.write_all(bytes)
                .await
                .map_err(|e| DeviceError::Io(std::io::Error::other(e.to_string())))
        })
        .await?;
        with_timeout("stage_file.shutdown", async {
            file.shutdown()
                .await
                .map_err(|e| DeviceError::Io(std::io::Error::other(e.to_string())))
        })
        .await?;
        drop(file);
        Ok(())
    }

    /// Promote a previously-staged file into place. Uses a backup-rename
    /// pattern because `russh-sftp` 2.1's plain rename refuses to overwrite
    /// (per the SFTP spec) and the SFTP server on the reMarkable ships
    /// without the posix-rename extension we'd otherwise prefer.
    ///
    /// 1. If `<path>` exists, rename it to `<path>.rehydrate-bak`.
    /// 2. Rename `<path>.rehydrate-tmp` → `<path>`.
    /// 3. Remove `<path>.rehydrate-bak`.
    ///
    /// If step 2 fails, restore by renaming the backup back into place so
    /// the device never sees a hole where the file used to be.
    ///
    /// Audit fix H2: pre-existing backups are conditionally cleaned.
    /// If `<path>` is missing but `<path>.rehydrate-bak` exists, a
    /// previous push must have hit a double-fault (step 2 failed AND
    /// the rollback rename also failed) — the only surviving copy of
    /// the user's file is the backup. Recover it instead of wiping it.
    async fn commit_staged(&self, sftp: &SftpSession, path: &str) -> DeviceResult<()> {
        let tmp_path = staged_path(path);
        let bak_path = backup_path(path);

        // Conditional pre-cleanup of any leftover backup.
        let live_existed = sftp.metadata(path).await.is_ok();
        let bak_existed = sftp.metadata(&bak_path).await.is_ok();
        match (live_existed, bak_existed) {
            (true, true) => {
                // Live exists; bak is leftover from a prior cycle that
                // forgot to clean up. Safe to drop.
                let _ = sftp.remove_file(&bak_path).await;
            }
            (false, true) => {
                // Live missing, bak present → recover bak as live
                // before staging. Surfaces a previous double-fault as
                // a recovery action rather than silent data loss.
                tracing::warn!(
                    path,
                    "previous push left {path}.rehydrate-bak with no live copy; recovering"
                );
                sftp.rename(&bak_path, path)
                    .await
                    .map_err(|e| sftp_err("recover(bak→live)", path, e))?;
            }
            _ => {}
        }

        // Re-check live presence after a possible recovery rename.
        let live_existed = sftp.metadata(path).await.is_ok();
        if live_existed {
            sftp.rename(path, &bak_path)
                .await
                .map_err(|e| sftp_err("rename(live→bak)", path, e))?;
        }

        match sftp.rename(&tmp_path, path).await {
            Ok(()) => {
                if live_existed {
                    let _ = sftp.remove_file(&bak_path).await;
                }
                Ok(())
            }
            Err(e) => {
                // Promotion failed. Try to restore the backup. If even
                // the rollback fails, surface a distinct error pointing
                // at the surviving bak so the user — and the next
                // push's recovery branch — can find it.
                if live_existed {
                    if let Err(roll_err) = sftp.rename(&bak_path, path).await {
                        return Err(DeviceError::Other(format!(
                            "promotion failed for {path} ({e}); rollback also failed ({roll_err}); \
                             previous file preserved at {bak_path}"
                        )));
                    }
                }
                Err(sftp_err("rename(tmp→live)", path, e))
            }
        }
    }

    /// Best-effort cleanup of staged tmps for a document — used when a
    /// document upload fails partway through, so we don't leave the
    /// device with `.rehydrate-tmp` litter.
    async fn discard_staged(&self, sftp: &SftpSession, paths: &[String]) {
        for path in paths {
            let _ = sftp.remove_file(&staged_path(path)).await;
        }
    }
}

fn staged_path(path: &str) -> String {
    format!("{path}.rehydrate-tmp")
}

fn backup_path(path: &str) -> String {
    format!("{path}.rehydrate-bak")
}

async fn open_sftp(handle: &Handle<ClientHandler>) -> DeviceResult<SftpSession> {
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

fn sftp_err(op: &str, path: &str, e: russh_sftp::client::error::Error) -> DeviceError {
    let msg = format!("{op} {path}: {e}");
    if msg.contains("NoSuchFile") || msg.contains("ENOENT") {
        DeviceError::NotFound(path.to_string())
    } else {
        DeviceError::Other(msg)
    }
}

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
                if name == "." || name == ".." {
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
        let subdir = format!("{dir}/{uuid}");
        match inner.sftp.read_dir(&subdir).await {
            Ok(_) => {
                fetch_subtree(&inner.sftp, &subdir, uuid, &mut out).await?;
            }
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("NoSuchFile") && !msg.contains("ENOENT") {
                    return Err(sftp_err("read_dir", &subdir, e));
                }
            }
        }

        // 3. Optional thumbnails directory: <xochitl>/<uuid>.thumbnails
        let thumb = format!("{dir}/{uuid}.thumbnails");
        if let Ok(_dir_entries) = inner.sftp.read_dir(&thumb).await {
            fetch_subtree_named(&inner.sftp, &thumb, &format!("{uuid}.thumbnails"), &mut out)
                .await?;
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

async fn read_path(sftp: &SftpSession, path: &str) -> DeviceResult<Vec<u8>> {
    let f = with_timeout("read_path.open", async {
        sftp.open(path).await.map_err(|e| sftp_err("open", path, e))
    })
    .await?;
    with_body_timeout("read_path.body", read_capped(f, path)).await
}

/// Read an SFTP file with a hard size limit. `take(MAX_REMOTE_FILE_BYTES + 1)`
/// lets us distinguish "file fits within budget" from "file exceeds budget"
/// without ever allocating beyond the cap.
async fn read_capped<R>(reader: R, path: &str) -> DeviceResult<Vec<u8>>
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

/// Quick TCP probe used by the connection watcher. Cheap; doesn't open SSH.
pub async fn is_reachable(host: &str, port: u16) -> bool {
    use std::time::Duration;
    let addr = format!("{host}:{port}");
    matches!(
        tokio::time::timeout(
            Duration::from_millis(800),
            tokio::net::TcpStream::connect(addr)
        )
        .await,
        Ok(Ok(_))
    )
}
