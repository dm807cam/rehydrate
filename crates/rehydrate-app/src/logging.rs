//! File-backed logging. Tracing writes to a daily-rotated log under the
//! platform's data directory so the operation log drawer in the UI can show
//! a tail of recent activity. Stderr output is preserved alongside, so
//! running the binary from a terminal still surfaces log lines live.

use std::path::PathBuf;
use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Holds the appender's worker thread guard for the lifetime of the process.
/// Dropping the guard would flush and stop the appender; we never want that.
static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Returns the directory where reHydrate writes log files.
pub fn log_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("app", "rehydrate", "reHydrate")
        .map(|d| d.data_local_dir().join("logs"))
}

pub fn init() {
    // Default filter: INFO globally, DEBUG for our own crates so sync
    // activity actually reaches the log file. The previous single
    // directive `rehydrate=debug` relied on prefix-matching our crate
    // names, but every workspace crate's module path is `rehydrate_app`,
    // `rehydrate_core`, … (underscored), not `rehydrate` — so in practice
    // it left the file effectively INFO-only. Listing each target
    // explicitly is verbose but unambiguous and self-documenting.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,\
             rehydrate_app=debug,\
             rehydrate_core=debug,\
             rehydrate_sync=debug,\
             rehydrate_device=debug,\
             rehydrate_render=debug,\
             rehydrate_ocr=debug,\
             rehydrate_publish=debug,\
             rehydrate_http=debug,\
             rm_parser=debug",
        )
    });

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true);

    // File appender. If we can't set up the log dir, fall back to stderr-
    // only — the app is still usable, but the Logs drawer will be empty.
    let file_layer = log_dir().and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        let appender = rolling::daily(&dir, "rehydrate.log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        // Stash the guard so the appender thread isn't dropped.
        let _ = LOG_GUARD.set(guard);
        Some(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_target(true)
                .boxed(),
        )
    });

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer);
    if let Some(file_layer) = file_layer {
        let _ = registry.with(file_layer).try_init();
    } else {
        let _ = registry.try_init();
    }
    // Trim any logs beyond the retention window. Done after init so any
    // surfaced errors land in the freshly-attached subscriber.
    prune_old_logs();
}

/// Maximum number of rotated log files we keep on disk. Anything older is
/// best-effort deleted by `prune_old_logs`. The appender rotates daily, so
/// this caps log retention at roughly a month.
const LOG_RETENTION_FILES: usize = 30;

/// Read up to `max_lines` most-recent lines across all rotated log files.
///
/// Reads files newest-first and stops as soon as enough lines have been
/// collected, so the caller never has to materialise the entire log
/// archive in memory just to show the last few hundred lines in the UI.
pub fn read_tail(max_lines: usize) -> std::io::Result<Vec<String>> {
    if max_lines == 0 {
        return Ok(Vec::new());
    }
    let Some(dir) = log_dir() else {
        return Ok(Vec::new());
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };

    // Sort entries by filename descending — daily rotation uses YYYY-MM-DD
    // suffixes so reverse-lex order is reverse-chronological.
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files.reverse();

    // Walk newest first; collect lines into per-file buckets so we can
    // splice them back together in chronological order at the end.
    let mut buckets: Vec<Vec<String>> = Vec::new();
    let mut collected = 0usize;
    for path in files {
        if collected >= max_lines {
            break;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let lines: Vec<String> = content.lines().map(str::to_owned).collect();
        let take_from = lines.len().saturating_sub(max_lines - collected);
        let chunk: Vec<String> = lines[take_from..].to_vec();
        collected += chunk.len();
        buckets.push(chunk);
    }

    // `buckets` is newest→oldest; flatten in reverse so the final order is
    // oldest→newest, matching what the UI expects to render.
    let mut out: Vec<String> = Vec::with_capacity(collected);
    for chunk in buckets.into_iter().rev() {
        out.extend(chunk);
    }
    Ok(out)
}

/// Best-effort removal of older log files so the archive does not grow
/// without bound. Failures are silent — log pruning is a maintenance
/// nicety, not a correctness requirement.
pub fn prune_old_logs() {
    let Some(dir) = log_dir() else { return };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    if files.len() <= LOG_RETENTION_FILES {
        return;
    }
    let drop_count = files.len() - LOG_RETENTION_FILES;
    for path in files.into_iter().take(drop_count) {
        let _ = std::fs::remove_file(&path);
    }
}
