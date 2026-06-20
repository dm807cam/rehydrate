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
//!
//! Submodule layout:
//!   - `handler`     — TOFU host-key flow (`ClientHandler` + `HostKeyOutcome`)
//!   - `io`          — timeout wrappers, SFTP helpers (open/read/error map),
//!                     tmp/bak naming, the size-capped reader, + sftp_err tests
//!   - `device_impl` — `impl Device for SshDevice` plus the `fetch_subtree*`
//!                     and `reap_*` helpers it uses
//!
//! This `mod.rs` owns the SSH-level state: `SshConfig`, the `SshDevice`
//! struct + its non-trait impl (constructors, stage/commit, exec,
//! purge_device_trash), and the bare `is_reachable` TCP probe.

mod device_impl;
mod handler;
mod io;

use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::error::{DeviceError, DeviceResult};
use crate::known_hosts::KnownHosts;
use crate::trait_def::Device;

use self::handler::{ClientHandler, HostKeyOutcome};
use self::io::{
    backup_path, open_sftp, read_capped, sftp_err, staged_path, with_body_timeout, with_timeout,
};

pub const DEFAULT_HOST: &str = "10.11.99.1";
pub const DEFAULT_PORT: u16 = 22;
pub const DEFAULT_USER: &str = "root";
pub const XOCHITL_DIR: &str = "/home/root/.local/share/remarkable/xochitl";

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

pub struct SshDevice {
    pub(super) cfg: SshConfig,
    pub(super) inner: Mutex<Inner>,
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

pub(super) struct Inner {
    /// SSH multiplexer for `exec(...)`. The trait impl in
    /// `device_impl.rs` never opens new channels — every cross-file
    /// operation goes through `sftp` — so the field stays
    /// module-private and the only consumer is `SshDevice::exec`.
    handle: Handle<ClientHandler>,
    pub(super) sftp: SftpSession,
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
            // Issue #33: refuse non-leaf entry names from the SFTP
            // server (`..`, embedded slashes, etc.). See
            // `crate::is_safe_entry_name`. Skip + warn rather than
            // error — this scan is best-effort observability, not
            // a load-bearing operation.
            if !crate::is_safe_entry_name(&name) {
                tracing::warn!(
                    parent = %dir,
                    entry = %name,
                    "device sent a non-leaf entry name; skipping (issue #33)",
                );
                continue;
            }
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
    pub(super) async fn exec(&self, cmd: &str) -> DeviceResult<String> {
        use russh::ChannelMsg;
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

    pub(super) async fn read_file(
        &self,
        sftp: &SftpSession,
        path: &str,
    ) -> DeviceResult<Vec<u8>> {
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
    pub(super) async fn stage_file(
        &self,
        sftp: &SftpSession,
        path: &str,
        bytes: &[u8],
    ) -> DeviceResult<()> {
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
    pub(super) async fn commit_staged(
        &self,
        sftp: &SftpSession,
        path: &str,
    ) -> DeviceResult<()> {
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
    pub(super) async fn discard_staged(&self, sftp: &SftpSession, paths: &[String]) {
        for path in paths {
            let _ = sftp.remove_file(&staged_path(path)).await;
        }
    }

    /// Hard-delete every document the tablet has soft-deleted (`deleted: true`
    /// in its `.metadata`). Returns the UUIDs of the removed documents so the
    /// caller can also clean up any corresponding local library entries.
    ///
    /// xochitl moves documents to its Trash view by writing `deleted: true`
    /// into the `.metadata` file rather than removing anything from disk.
    /// Those files accumulate indefinitely until the user manually empties
    /// the Trash from the tablet UI. This method does that hard-deletion over
    /// SFTP so the user can reclaim space without touching the tablet.
    pub async fn purge_device_trash(&self) -> DeviceResult<Vec<String>> {
        let dir = self.cfg.xochitl_dir.clone();

        // Phase 1: scan .metadata files and collect soft-deleted UUIDs.
        // Hold the lock for the whole scan so we get a consistent snapshot,
        // then release it before calling delete_document_tree (which takes
        // the lock per-UUID).
        let deleted_uuids: Vec<String> = {
            let inner = self.inner.lock().await;
            let entries = with_timeout("purge_trash.read_dir", async {
                inner
                    .sftp
                    .read_dir(&dir)
                    .await
                    .map_err(|e| sftp_err("read_dir", &dir, e))
            })
            .await?;

            let mut out = Vec::new();
            for e in entries {
                let name = e.file_name();
                if !crate::is_safe_entry_name(&name) {
                    continue;
                }
                let Some(uuid) = name.strip_suffix(".metadata") else {
                    continue;
                };
                let path = format!("{dir}/{name}");
                let Ok(bytes) = self.read_file(&inner.sftp, &path).await else {
                    continue;
                };
                let Ok(meta) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                if meta
                    .get("deleted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    out.push(uuid.to_string());
                }
            }
            out
        }; // lock released here

        // Phase 2: hard-delete each UUID's full file tree.
        for uuid in &deleted_uuids {
            if let Err(e) = self.delete_document_tree(uuid).await {
                tracing::warn!(
                    uuid = %uuid,
                    error = %e,
                    "purge_device_trash: could not delete"
                );
            }
        }

        let count = deleted_uuids.len();
        tracing::info!(count, "purge_device_trash: hard-deleted soft-deleted documents");
        if count > 0 {
            if let Err(e) = self.refresh_document_index().await {
                tracing::warn!(error = %e, "purge_device_trash: index refresh failed");
            }
        }
        Ok(deleted_uuids)
    }
}

/// Quick TCP probe used by the connection watcher. Cheap; doesn't open SSH.
pub async fn is_reachable(host: &str, port: u16) -> bool {
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
