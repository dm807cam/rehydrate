use std::sync::Arc;

use rehydrate_device::ssh::SshDevice;
use rehydrate_sync::{
    execute_pull, execute_push, plan_pull, plan_push, progress, ProgressEvent, PullPlan, PushPlan,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::state::AppState;
use crate::util::{err, lib_arc};

#[derive(Serialize)]
pub struct SyncReportOut {
    pub recorded: usize,
    pub unchanged: usize,
    pub skipped: usize,
}

#[derive(Serialize)]
pub struct PushReportOut {
    pub pushed: usize,
    pub unchanged: usize,
    pub skipped: usize,
}

#[derive(Serialize)]
pub struct TwoWayReport {
    pub pull: SyncReportOut,
    pub push: PushReportOut,
}

#[tauri::command]
pub async fn restore_version(version_id: i64, state: State<'_, AppState>) -> Result<i64, String> {
    let lib = lib_arc(&state).await?;
    let outcome = lib.restore_version(version_id).map_err(err)?;
    Ok(outcome.version_id)
}

async fn device_arc(state: &State<'_, AppState>) -> Result<Arc<SshDevice>, String> {
    state
        .device
        .lock()
        .await
        .as_ref()
        .map(Arc::clone)
        .ok_or_else(|| "device not connected".to_string())
}

/// Forward sync progress events from the engine's channel to the
/// renderer over the Tauri event bus. Bails out cleanly when:
///
/// - the engine drops its sender (channel closes), OR
/// - the engine signals `Done`/`Cancelled`, OR
/// - the renderer disconnects (consecutive `emit` failures past
///   `EMIT_FAIL_THRESHOLD`) — without this guard, the forwarder
///   would spin on a dead IPC channel until the engine itself
///   completed.
///
/// Same pattern across pull/push/two-way, so it lives here rather
/// than copy-pasted.
fn spawn_sync_progress_forwarder(
    app: AppHandle,
    mut rx: tokio::sync::mpsc::Receiver<ProgressEvent>,
) -> tauri::async_runtime::JoinHandle<()> {
    const EMIT_FAIL_THRESHOLD: u32 = 8;
    tauri::async_runtime::spawn(async move {
        let mut consecutive_emit_failures: u32 = 0;
        while let Some(ev) = rx.recv().await {
            match app.emit("sync:progress", &ev) {
                Ok(()) => consecutive_emit_failures = 0,
                Err(e) => {
                    consecutive_emit_failures += 1;
                    if consecutive_emit_failures >= EMIT_FAIL_THRESHOLD {
                        tracing::warn!(
                            "sync progress forwarder bailing after {consecutive_emit_failures} \
                             consecutive emit failures (last: {e}); renderer likely closed"
                        );
                        break;
                    }
                }
            }
            if matches!(ev, ProgressEvent::Done { .. } | ProgressEvent::Cancelled) {
                break;
            }
        }
    })
}

#[tauri::command]
pub async fn pull_plan(state: State<'_, AppState>) -> Result<PullPlan, String> {
    let dev = device_arc(&state).await?;
    let lib = lib_arc(&state).await?;
    plan_pull(&lib, dev.as_ref()).await.map_err(err)
}

#[tauri::command]
pub async fn pull_execute(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SyncReportOut, String> {
    let dev = device_arc(&state).await?;
    let lib = lib_arc(&state).await?;

    let plan = plan_pull(&lib, dev.as_ref()).await.map_err(err)?;

    let (tx, rx) = progress::channel(64);
    let forwarder = spawn_sync_progress_forwarder(app.clone(), rx);

    // Reset the shared cancel handle to a clean (un-cancelled) state
    // and pass a clone to the engine. The renderer's `cancel_sync`
    // command flips the same handle.
    state.sync_cancel.reset();
    let report = execute_pull(
        &lib,
        dev.as_ref(),
        plan,
        Some(tx),
        state.sync_cancel.clone(),
    )
    .await
    .map_err(err)?;
    let _ = forwarder.await;

    Ok(SyncReportOut {
        recorded: report.recorded,
        unchanged: report.unchanged,
        skipped: report.skipped,
    })
}

#[tauri::command]
pub async fn push_plan(state: State<'_, AppState>) -> Result<PushPlan, String> {
    let lib = lib_arc(&state).await?;
    plan_push(&lib).map_err(err)
}

#[tauri::command]
pub async fn push_execute(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<PushReportOut, String> {
    let dev = device_arc(&state).await?;
    let lib = lib_arc(&state).await?;

    let plan = plan_push(&lib).map_err(err)?;
    let (tx, rx) = progress::channel(64);
    let forwarder = spawn_sync_progress_forwarder(app.clone(), rx);
    state.sync_cancel.reset();
    let report = execute_push(
        &lib,
        dev.as_ref(),
        plan,
        Some(tx),
        state.sync_cancel.clone(),
    )
    .await
    .map_err(err)?;
    let _ = forwarder.await;

    Ok(PushReportOut {
        pushed: report.pushed,
        unchanged: report.unchanged,
        skipped: report.skipped,
    })
}

/// Pull-then-push. The pull-first ordering means device-side changes are
/// captured before any library-side change overwrites them; the loser of any
/// conflict is preserved in the version log via parent_version_id and
/// remains restorable from the history view.
#[tauri::command]
pub async fn sync_two_way(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<TwoWayReport, String> {
    let dev = device_arc(&state).await?;
    let lib = lib_arc(&state).await?;

    // Reset the cancel handle once for the whole two-way sync; the
    // renderer's `cancel_sync` command will trip both phases.
    state.sync_cancel.reset();

    // ----- PULL phase -----
    let _ = app.emit("sync:phase", "pull");
    let pull_plan = plan_pull(&lib, dev.as_ref()).await.map_err(err)?;
    let (tx, rx) = progress::channel(64);
    let forwarder = spawn_sync_progress_forwarder(app.clone(), rx);
    let pull = execute_pull(
        &lib,
        dev.as_ref(),
        pull_plan,
        Some(tx),
        state.sync_cancel.clone(),
    )
    .await
    .map_err(err)?;
    let _ = forwarder.await;

    // ----- PUSH phase -----
    let _ = app.emit("sync:phase", "push");
    let push_plan = plan_push(&lib).map_err(err)?;
    let (tx, rx) = progress::channel(64);
    let forwarder = spawn_sync_progress_forwarder(app.clone(), rx);
    let push = execute_push(
        &lib,
        dev.as_ref(),
        push_plan,
        Some(tx),
        state.sync_cancel.clone(),
    )
    .await
    .map_err(err)?;
    let _ = forwarder.await;

    Ok(TwoWayReport {
        pull: SyncReportOut {
            recorded: pull.recorded,
            unchanged: pull.unchanged,
            skipped: pull.skipped,
        },
        push: PushReportOut {
            pushed: push.pushed,
            unchanged: push.unchanged,
            skipped: push.skipped,
        },
    })
}
