use std::time::Instant;

use rehydrate_ocr::default_model_id;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::config;
use crate::state::{AppState, OllamaPing, OLLAMA_PING_TTL};
use crate::util::err;

/// `OllamaConfig` re-exported as the IPC DTO. The struct already
/// derives `Serialize + Deserialize`, and using it directly keeps
/// the Rust and TS sides in lockstep — adding a new field touches
/// `config.rs` and the TS interface only.
pub type OllamaConfigDto = config::OllamaConfig;

#[derive(Serialize)]
pub struct PingReport {
    pub ok: bool,
    pub error: Option<String>,
    /// Names of models the daemon reports via `/api/tags`. The UI uses
    /// this to confirm that the model dropdown's selection has
    /// actually been pulled.
    pub models: Vec<String>,
}

/// Curated list of recommended models surfaced in the Settings UI.
/// The UI shows these as the primary options; a "Custom…" row lets
/// the user enter anything else they've pulled.
#[derive(Serialize)]
pub struct CuratedOllamaModel {
    pub id: String,
    pub label: String,
    pub vram_hint: &'static str,
}

#[tauri::command]
pub async fn get_ollama_config() -> Result<OllamaConfigDto, String> {
    Ok(config::load().ollama)
}

#[tauri::command]
pub async fn save_ollama_config(cfg: OllamaConfigDto) -> Result<(), String> {
    validate_ollama_url(&cfg.base_url)?;
    if cfg.model.trim().is_empty() {
        return Err("Pick a model from the list or enter a custom name.".into());
    }
    let mut on_disk = config::load();
    on_disk.ollama = cfg;
    config::save(&on_disk).map_err(err)
}

/// Reject URLs the user shouldn't be pointing OCR at.
///
/// The actual rules — scheme/loopback/private-IP gating — live in
/// `rehydrate_ocr::validate_remote_url`, which is also what
/// `RestrictedAgent::for_base` calls before constructing the ureq
/// agent. Keeping the gate in one place means the IPC probe and the
/// production fetch always see the same answer; a renderer can't
/// bypass the gate by going through a different code path.
pub(super) fn validate_ollama_url(url_str: &str) -> Result<(), String> {
    rehydrate_ocr::validate_remote_url(url_str).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ping_ollama(
    base_url: String,
    state: State<'_, AppState>,
) -> Result<PingReport, String> {
    let report = probe_ollama(base_url.clone()).await?;
    // Every probe — whether from Settings → Test connection, from
    // ocr_status, or from an internal call — must refresh the
    // shared reachability cache. Without this the user can verify
    // in Settings that the daemon is up, return to the library,
    // hit Convert, and still be told it's unreachable for the
    // full 30s cache TTL.
    record_ping(&state, &base_url, report.ok).await;
    Ok(report)
}

/// Internal probe that performs the HTTP call without touching
/// the cache. Lets `ocr_status` re-use the same logic while only
/// recording once at its own call site (and lets the test suite
/// exercise the network shape without a `State` handle).
async fn probe_ollama(base_url: String) -> Result<PingReport, String> {
    // Same scheme + loopback gate the save path enforces — probing
    // bypasses save, so without this the renderer could trigger
    // requests to e.g. cloud-metadata endpoints just by calling
    // ping_ollama with a crafted URL.
    if let Err(msg) = validate_ollama_url(&base_url) {
        return Ok(PingReport {
            ok: false,
            error: Some(msg),
            models: Vec::new(),
        });
    }
    // Spawn-blocking because ureq is sync. Short timeout for the
    // probe — the UI is waiting on this.
    tauri::async_runtime::spawn_blocking(move || -> PingReport {
        let agent = match rehydrate_ocr::RestrictedAgent::for_base_with_timeout(
            &base_url,
            std::time::Duration::from_secs(5),
        ) {
            Ok(a) => a,
            Err(e) => {
                return PingReport {
                    ok: false,
                    error: Some(format!("{e}")),
                    models: Vec::new(),
                }
            }
        };
        let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
        match agent.get(&url, &[]) {
            Ok(resp) if resp.status == 200 => {
                match serde_json::from_str::<TagsResponse>(&resp.body) {
                    Ok(parsed) => PingReport {
                        ok: true,
                        error: None,
                        models: parsed.models.into_iter().map(|m| m.name).collect(),
                    },
                    Err(e) => PingReport {
                        // Daemon answered with 200 — still reachable
                        // even if the body wasn't the JSON we expected
                        // (older / forked builds, proxies). Treat as
                        // reachable so we don't loop back to "Ollama
                        // unreachable"; the empty model list surfaces
                        // a clear "Model not pulled" path instead.
                        ok: true,
                        error: Some(format!(
                            "connected, but /api/tags returned unexpected JSON: {e}"
                        )),
                        models: Vec::new(),
                    },
                }
            }
            Ok(resp) => PingReport {
                ok: false,
                error: Some(format!("HTTP {}", resp.status)),
                models: Vec::new(),
            },
            Err(e) => PingReport {
                ok: false,
                error: Some(format!("{e}")),
                models: Vec::new(),
            },
        }
    })
    .await
    .map_err(err)
}

#[tauri::command]
pub async fn list_curated_ollama_models() -> Result<Vec<CuratedOllamaModel>, String> {
    // Qwen 3.5 (released ~1 month before v1.0.0) supersedes the
    // Qwen3-VL line. Upstream benchmarks: OCRBench 93.1% and
    // OmniDocBench1.5 90.8% — both directly relevant to the
    // handwritten-notebook workload. Two curated tiers keep the
    // dropdown manageable; the "Custom…" option in the picker
    // covers users who pull a different model.
    Ok(vec![
        CuratedOllamaModel {
            id: "qwen3.5:4b".into(),
            label: "Qwen 3.5 4B — default, fast".into(),
            vram_hint: "~4 GB VRAM / Apple Silicon unified memory",
        },
        CuratedOllamaModel {
            id: "qwen3.5:9b".into(),
            label: "Qwen 3.5 9B — sharper at cursive + math".into(),
            vram_hint: "~7 GB VRAM recommended",
        },
    ])
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagsModel>,
}

#[derive(Deserialize)]
struct TagsModel {
    name: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OcrStatusReport {
    /// The configured Ollama URL doesn't respond. UI auto-opens the
    /// Settings modal on the Ollama tab.
    Unreachable {
        base_url: String,
        model: String,
        error: String,
    },
    /// Daemon responds but doesn't have the configured model pulled.
    /// UI surfaces an explainer + a copy-paste `ollama pull` command.
    ModelMissing {
        base_url: String,
        model: String,
        available: Vec<String>,
    },
    /// Ready to transcribe.
    Ready { base_url: String, model: String },
}

#[tauri::command]
pub async fn ocr_status(state: State<'_, AppState>) -> Result<OcrStatusReport, String> {
    let ollama = config::load().ollama;
    let base = ollama.base_url.clone();
    let model = ollama.model.clone();
    let probe = probe_ollama(base.clone()).await?;
    // Always record reachability — daemon answered or not — so a
    // missing-model branch below doesn't lock the reachability
    // cache into a false negative.
    record_ping(&state, &base, probe.ok).await;
    if !probe.ok {
        return Ok(OcrStatusReport::Unreachable {
            base_url: base,
            model,
            error: probe.error.unwrap_or_else(|| "unreachable".into()),
        });
    }
    let model_present = probe.models.iter().any(|m| m == &model);
    if model_present {
        Ok(OcrStatusReport::Ready {
            base_url: base,
            model,
        })
    } else {
        Ok(OcrStatusReport::ModelMissing {
            base_url: base,
            model,
            available: probe.models,
        })
    }
}

/// Helpers take `&AppState` rather than `State<'_, AppState>` so
/// the cache semantics are unit-testable without a Tauri runtime.
/// Tauri's `State<T>` derefs to `&T`, so call sites pass `&state`
/// from the IPC handlers unchanged.
pub(super) async fn record_ping(state: &AppState, base_url: &str, reachable: bool) {
    *state.last_ollama_ping.write().await = Some(OllamaPing {
        at: Instant::now(),
        base_url: base_url.to_string(),
        reachable,
    });
}

/// Returns the cached reachability of the daemon at `base_url`, or
/// `None` if there's no recent probe to consult. Strictly
/// reachability — the caller still has to handle "reachable but
/// model missing" separately.
pub(super) async fn cached_reachable(state: &AppState, base_url: &str) -> Option<bool> {
    let guard = state.last_ollama_ping.read().await;
    let p = guard.as_ref()?;
    if p.base_url != base_url {
        return None;
    }
    if p.at.elapsed() > OLLAMA_PING_TTL {
        return None;
    }
    Some(p.reachable)
}

#[tauri::command]
pub async fn default_ollama_model() -> Result<String, String> {
    Ok(default_model_id().to_string())
}

#[cfg(test)]
mod ollama_url_tests {
    use super::validate_ollama_url;

    #[test]
    fn accepts_localhost_http() {
        validate_ollama_url("http://localhost:11434").unwrap();
        validate_ollama_url("http://127.0.0.1:11434").unwrap();
        validate_ollama_url("http://[::1]:11434").unwrap();
    }

    #[test]
    fn accepts_remote_https_named() {
        validate_ollama_url("https://ollama.example.com").unwrap();
    }

    #[test]
    fn rejects_https_to_private_ip() {
        // A renderer XSS that flipped the configured base URL to one
        // of these could exfiltrate page imagery to an internal host
        // the user never intended; the audit specifically flagged
        // `https://10.0.0.5` and `https://169.254.169.254` as
        // SSRF-adjacent targets that previously slipped through.
        assert!(validate_ollama_url("https://10.0.0.5:11434").is_err());
        assert!(validate_ollama_url("https://169.254.169.254").is_err());
        assert!(validate_ollama_url("https://192.168.1.10:11434").is_err());
    }

    #[test]
    fn rejects_non_loopback_plain_http() {
        // The classic LAN-Ollama misconfiguration that would
        // otherwise leak page images and transcripts in plaintext.
        assert!(validate_ollama_url("http://10.0.0.5:11434").is_err());
        assert!(validate_ollama_url("http://ollama.example.com").is_err());
        assert!(validate_ollama_url("http://192.168.1.10:11434").is_err());
    }

    #[test]
    fn rejects_non_http_schemes() {
        // ureq would do something unexpected with these.
        assert!(validate_ollama_url("file:///etc/passwd").is_err());
        assert!(validate_ollama_url("gopher://example.com").is_err());
    }

    #[test]
    fn rejects_malformed_urls() {
        assert!(validate_ollama_url("").is_err());
        assert!(validate_ollama_url("not a url").is_err());
        assert!(validate_ollama_url("http://").is_err());
    }
}

#[cfg(test)]
mod reachability_cache_tests {
    //! Cache-semantics regressions. Three bugs cohabited the
    //! earlier version of this file:
    //!
    //! 1. `OllamaPing::ok` was overloaded — `ocr_status` wrote
    //!    `model_present` into the same field `transcribe_document`
    //!    later read as "daemon reachable". A missing model thus
    //!    locked the cache into a false negative for 30 s.
    //!
    //! 2. `ping_ollama` (the Settings → Test Connection IPC
    //!    handler) didn't update the cache at all, so a user
    //!    verifying the connection in Settings could not clear a
    //!    stale negative reading.
    //!
    //! 3. The cache then short-circuited the next transcribe with
    //!    "Ollama unreachable" while Settings simultaneously said
    //!    everything was fine — exactly the symptom the user
    //!    reported.
    //!
    //! These tests pin the contract that prevents the regression.
    use super::*;
    use crate::state::AppState;

    #[tokio::test]
    async fn record_ping_persists_reachable_flag_per_base_url() {
        let state = AppState::new();
        assert_eq!(
            cached_reachable(&state, "http://localhost:11434").await,
            None
        );

        record_ping(&state, "http://localhost:11434", true).await;
        assert_eq!(
            cached_reachable(&state, "http://localhost:11434").await,
            Some(true),
        );

        // Switching base URL invalidates the cached reading — the
        // user may have edited Settings between probes.
        assert_eq!(cached_reachable(&state, "http://remote:11434").await, None);

        record_ping(&state, "http://localhost:11434", false).await;
        assert_eq!(
            cached_reachable(&state, "http://localhost:11434").await,
            Some(false),
        );
    }

    #[tokio::test]
    async fn a_successful_probe_clears_a_prior_unreachable_cache() {
        // Models the user's reported flow: an earlier OCR run wrote
        // `reachable: false` (network blip, daemon restart, etc.),
        // then the user hit Settings → Test Connection and saw a
        // success. The next OCR start must NOT short-circuit on
        // the stale negative reading — `ping_ollama` is required
        // to refresh the cache from any call site, which means a
        // subsequent `record_ping(true)` overwrites the prior
        // `false`. The bug was that `ping_ollama` never wrote to
        // the cache, leaving the old `false` in place.
        let state = AppState::new();
        record_ping(&state, "http://localhost:11434", false).await;
        assert_eq!(
            cached_reachable(&state, "http://localhost:11434").await,
            Some(false),
        );

        // Settings' Test Connection now succeeds → must replace
        // the negative reading, not coexist with it.
        record_ping(&state, "http://localhost:11434", true).await;
        assert_eq!(
            cached_reachable(&state, "http://localhost:11434").await,
            Some(true),
            "a fresh successful probe must overwrite a stale negative cache",
        );
    }
}
