use std::sync::Arc;

use rehydrate_core::ArchiveReason;
use rehydrate_device::known_hosts::KnownHosts;
use rehydrate_device::ssh::{is_reachable, SshConfig, SshDevice};
use rehydrate_device::{Device, DeviceInfo};
use secrecy::SecretString;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::keychain;
use crate::state::{AppState, KEYRING_DEVICE_USER};
use crate::util::{err, lib_arc, IpcSecret};

#[derive(Serialize)]
pub struct DeviceState {
    pub reachable: bool,
    pub connected: bool,
    pub info: Option<DeviceInfo>,
    /// True if a password is stored in the OS keychain — the UI uses this
    /// to decide whether to show a password prompt or just a Connect button.
    pub has_stored_password: bool,
    /// True if the host-key TOFU store has a pinned fingerprint for the
    /// default device endpoint. Drives the "Forget host key" affordance
    /// in the StatusPill popover — without this the user can only clear
    /// a stale pin by editing `known_hosts.json` by hand.
    pub has_recorded_host_key: bool,
}

/// Resolve the per-user known-hosts store for SSH host-key pinning.
/// Falls back to a cwd-relative path if the OS doesn't expose a
/// config directory — the file then lives next to the binary, which
/// is functional but not pretty (and very unusual in practice; every
/// platform we support exposes a config dir).
fn known_hosts_for_app() -> KnownHosts {
    let dir = directories::ProjectDirs::from("app", "rehydrate", "reHydrate")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    KnownHosts::new(dir.join("known_hosts.json"))
}

#[tauri::command]
pub async fn device_state(state: State<'_, AppState>) -> Result<DeviceState, String> {
    let cfg = SshConfig::default();
    let endpoint = format!("{}:{}", cfg.host, cfg.port);
    // A missing or unreadable known_hosts file is reported as "no
    // recorded fingerprint" — the UI will simply hide the Forget
    // affordance, which is the right post-state either way.
    let has_recorded_host_key = known_hosts_for_app()
        .lookup(&endpoint)
        .ok()
        .flatten()
        .is_some();
    Ok(DeviceState {
        reachable: *state.device_reachable.read().await,
        connected: state.device.lock().await.is_some(),
        info: state.device_info.read().await.clone(),
        has_stored_password: keychain::slot_has_value(KEYRING_DEVICE_USER),
        has_recorded_host_key,
    })
}

/// Clear the pinned host-key fingerprint for the default device
/// endpoint. The next connect will re-record under TOFU.
///
/// Returns `true` when an entry was removed and `false` when none
/// existed — both outcomes leave the UI in the same desired state
/// ("no pinned key"), so the renderer treats both as success and
/// just refreshes `device_state` to hide the button.
#[tauri::command]
pub async fn forget_device_host_key() -> Result<bool, String> {
    let cfg = SshConfig::default();
    let endpoint = format!("{}:{}", cfg.host, cfg.port);
    known_hosts_for_app().forget(&endpoint).map_err(err)
}

/// Save a device password into the OS keychain. Does not connect.
/// The `IpcSecret` wrapper length-caps the input and ensures the
/// plaintext isn't printable via Debug derives further up the
/// call chain.
#[tauri::command]
pub async fn save_device_password(password: IpcSecret) -> Result<(), String> {
    keychain::write_slot(KEYRING_DEVICE_USER, password.expose())
}

#[tauri::command]
pub async fn forget_device_password() -> Result<(), String> {
    keychain::forget_slot(KEYRING_DEVICE_USER)
}

/// Open an SSH session against the tablet.
///
/// `password` semantics:
/// - `None`: read the password from the OS keychain. If none stored,
///   returns an error so the renderer can show the password dialog.
/// - `Some(_)`: use the supplied password.
///
/// `remember` semantics:
/// - `Some(true)`: on a successful connect, persist `password` to the
///   keychain. Requires `password` to be `Some(_)`.
/// - Any other value: do **not** write the keychain.
///
/// The previous default was "always save on first successful
/// connect". The renderer is the choke point — if it's XSS'd, both
/// `save_device_password` and `connect_device` are reachable
/// programmatically. Requiring an explicit `remember: true` flag
/// means a hostile script that just wants to quietly probe
/// `connect_device` (e.g. to confirm a tablet is online) can no
/// longer overwrite the user's stored password as a side effect.
/// The dedicated `save_device_password` command remains the
/// canonical "store this" call.
#[tauri::command]
pub async fn connect_device(
    password: Option<IpcSecret>,
    remember: Option<bool>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DeviceInfo, String> {
    let cfg = SshConfig::default();
    // Resolve the password without persisting yet. Persisting before we
    // know the password is correct means a typo gets cached and the next
    // connect attempt silently uses the bad value.
    //
    // Audit fix M4: a parallel un-zeroized `Option<String>` shadow used
    // to defeat SecretString's zero-on-drop. The IPC layer now hands us
    // an `IpcSecret`, so the plaintext never lives in a plain String the
    // caller could Debug-print. We unwrap into SecretString immediately.
    let (secret, freshly_typed) = match password {
        Some(p) => (p.into_secret(), true),
        None => {
            let stored = keychain::read_slot(KEYRING_DEVICE_USER)
                .ok_or_else(|| "no password stored; pass one to connect_device".to_string())?;
            (SecretString::from(stored), false)
        }
    };

    let dev = SshDevice::connect(cfg, secret.clone(), known_hosts_for_app())
        .await
        .map_err(err)?;
    let info = dev.ping().await.map_err(err)?;

    // Surface any one-shot warnings the connect path collected.
    // Today the only case is "TOFU host-key write failed" — see
    // `SshDevice::take_pending_warning`. The connection is fine to
    // use; the warning tells the user that subsequent reconnects
    // won't have a pinned fingerprint to compare against until
    // they fix the underlying FS / permissions issue.
    if let Some(msg) = dev.take_pending_warning() {
        tracing::warn!("device pending warning: {msg}");
        let _ = app.emit("host-key:warning", msg);
    }

    // Connection succeeded — persist only if the renderer explicitly
    // asked for it. If the OS keyring is unavailable (most often on
    // minimal Linux installs without `secret-service` /
    // `gnome-keyring` running), both `Entry::new` and `set_password`
    // can fail. We surface the failure as a `keyring:warning` event
    // so the UI can tell the user "we couldn't save your password";
    // silently logging makes the user wonder why their password
    // isn't being remembered.
    if freshly_typed && remember == Some(true) {
        use secrecy::ExposeSecret;
        if let Err(e) = keychain::write_slot(KEYRING_DEVICE_USER, secret.expose_secret()) {
            tracing::warn!("could not persist device password to keychain: {e}");
            let _ = app.emit(
                "keyring:warning",
                format!(
                    "Couldn't save the tablet password to the system keychain ({e}). \
                     You'll be asked again next time. On Linux this usually means \
                     `gnome-keyring` / `secret-service` isn't installed or running."
                ),
            );
        }
    }

    *state.device.lock().await = Some(Arc::new(dev));
    *state.device_info.write().await = Some(info.clone());
    Ok(info)
}

#[tauri::command]
pub async fn disconnect_device(state: State<'_, AppState>) -> Result<(), String> {
    *state.device.lock().await = None;
    *state.device_info.write().await = None;
    Ok(())
}

/// Hard-delete every document in the tablet's Trash (those with
/// `deleted: true` in their `.metadata`). Returns the count of UUIDs removed.
///
/// After removing the files from the device, also cleans up any
/// corresponding entries in the local library so the "Tablet Trash" view
/// empties immediately without requiring a follow-up sync.
#[tauri::command]
pub async fn purge_device_trash(state: State<'_, AppState>) -> Result<usize, String> {
    let guard = state.device.lock().await;
    let dev = guard
        .as_ref()
        .ok_or_else(|| "not connected to a device".to_string())?;
    let purged = dev.purge_device_trash().await.map_err(err)?;
    let count = purged.len();
    drop(guard); // release device lock before taking library lock

    // Remove the purged UUIDs from the local library so the app's
    // "Tablet Trash" view reflects the change immediately. For each UUID:
    //   • archive_document: moves a live doc to archived_documents (no-op
    //     if the doc isn't in the live table or is already archived).
    //   • purge_archived_document: permanently removes it from archive.
    // Both errors are silently ignored — if the UUID was never in the
    // local library at all, there's nothing to clean up.
    if !purged.is_empty() {
        if let Ok(lib) = lib_arc(&state).await {
            for uuid in &purged {
                let _ = lib.archive_document(uuid, ArchiveReason::Device);
                let _ = lib.purge_archived_document(uuid);
            }
        }
    }
    Ok(count)
}

/// Background task started at app launch. Polls the USB-ethernet endpoint
/// every 2s and emits `device:reachable` events on changes. Cheap and
/// platform-agnostic — no USB driver hooks needed.
///
/// The loop watches `AppState::shutdown_requested` and exits when the
/// app's `RunEvent::ExitRequested` handler sets it — without the flag,
/// the thread holds the `AppHandle` past Tauri's teardown and prevents
/// clean shutdown of the executor.
pub fn spawn_reachability_watcher(app: AppHandle) {
    // Run on a dedicated OS thread with its own tokio current-thread
    // runtime. Doing this from the Tauri `setup` callback fails because
    // neither tokio nor `tauri::async_runtime` has its reactor live yet
    // — they spin up later in Builder::run(). A standalone thread sidesteps
    // the ordering question entirely. The work is tiny (one TCP connect
    // every 2s), so a private runtime is fine.
    std::thread::Builder::new()
        .name("rehydrate-reachability".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("failed to start reachability watcher runtime: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                let mut last: Option<bool> = None;
                // Snapshot the shutdown flag once; cloning the Arc
                // up front keeps the per-tick check off the AppHandle.
                let shutdown = {
                    let state = app.state::<AppState>();
                    state.shutdown_requested.clone()
                };
                loop {
                    if shutdown.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let now = is_reachable("10.11.99.1", 22).await;
                    if last != Some(now) {
                        let state = app.state::<AppState>();
                        *state.device_reachable.write().await = now;
                        let _ = app.emit("device:reachable", now);
                        last = Some(now);
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            });
        })
        .expect("spawn reachability watcher thread");
}
