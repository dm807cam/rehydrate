use rehydrate_core::{Manifest, VersionId};
use rehydrate_publish::{
    host_of, DraftPost, GhostClient, GhostCredentials, PublishResult, PublishTarget, Publisher,
    WordpressClient, WordpressCredentials,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use super::transcribe::strip_frontmatter;
use super::TRANSCRIPT_PATH;
use crate::keychain;
use crate::state::{AppState, KEYRING_GHOST_CREDS, KEYRING_WORDPRESS_CREDS};
use crate::util::{err, lib_arc};

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublishKind {
    Ghost,
    Wordpress,
}

impl PublishKind {
    fn target(&self) -> PublishTarget {
        match self {
            PublishKind::Ghost => PublishTarget::Ghost,
            PublishKind::Wordpress => PublishTarget::Wordpress,
        }
    }
}

/// Tagged "no credentials" error used by the renderer to route the
/// user to Settings → Publishing. Mirrors `unconfigured_error` for
/// Ollama so the UI can use one error-shape pattern for both.
fn publishing_unconfigured_error(target: &str) -> String {
    let v = serde_json::json!({
        "kind": "publish_unconfigured",
        "target": target,
        "message": format!(
            "No {target} credentials saved. Open Settings → Publishing to add them."
        ),
    });
    v.to_string()
}

#[tauri::command]
pub async fn publish_transcript(
    state: State<'_, AppState>,
    version_id: VersionId,
    target: PublishKind,
) -> Result<PublishResult, String> {
    let lib = lib_arc(&state).await?;
    let bytes = lib
        .read_derived_artefact(version_id, TRANSCRIPT_PATH)
        .map_err(err)?
        .ok_or_else(|| "no transcript on this version".to_string())?;
    let md = String::from_utf8_lossy(&bytes).into_owned();
    let body = strip_frontmatter(&md);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, pulldown_cmark::Parser::new(&body));

    let entry = lib.get_version(version_id).map_err(err)?;
    let manifest_bytes = lib.read_blob(&entry.manifest_hash).map_err(err)?;
    let manifest = Manifest::from_canonical_json(&manifest_bytes).map_err(err)?;

    let post = DraftPost {
        title: manifest.visible_name.clone(),
        html,
        tags: vec!["from-rehydrate".into()],
    };

    let kind = target.target();
    let target_label = match kind {
        PublishTarget::Ghost => "ghost",
        PublishTarget::Wordpress => "wordpress",
    };
    tauri::async_runtime::spawn_blocking(move || -> Result<PublishResult, String> {
        let publisher: Box<dyn Publisher> = match kind {
            PublishTarget::Ghost => match load_ghost_client() {
                Ok(c) => Box::new(c),
                Err(_) => return Err(publishing_unconfigured_error(target_label)),
            },
            PublishTarget::Wordpress => match load_wordpress_client() {
                Ok(c) => Box::new(c),
                Err(_) => return Err(publishing_unconfigured_error(target_label)),
            },
        };
        publisher.publish_draft(&post).map_err(err)
    })
    .await
    .map_err(err)?
}

fn load_ghost_client() -> Result<GhostClient, String> {
    let json = keychain::read_slot(KEYRING_GHOST_CREDS)
        .ok_or_else(|| "no Ghost credentials saved".to_string())?;
    let creds: GhostCredentials = serde_json::from_str(&json).map_err(err)?;
    GhostClient::new(creds).map_err(err)
}

fn load_wordpress_client() -> Result<WordpressClient, String> {
    let json = keychain::read_slot(KEYRING_WORDPRESS_CREDS)
        .ok_or_else(|| "no WordPress credentials saved".to_string())?;
    let creds: WordpressCredentials = serde_json::from_str(&json).map_err(err)?;
    WordpressClient::new(creds).map_err(err)
}

#[tauri::command]
pub async fn set_ghost_credentials(creds: GhostCredentials) -> Result<(), String> {
    // Validate before the credentials hit the keychain so a misshaped
    // URL never leaves the IPC layer. `validate_remote_url` blocks
    // `http://` to public hosts and any literal-IP private/link-local
    // target — both of which would leak the Admin API key in
    // cleartext or pivot to internal services.
    rehydrate_publish::validate_remote_url(&creds.base_url).map_err(err)?;
    let json = serde_json::to_string(&creds).map_err(err)?;
    keychain::write_slot(KEYRING_GHOST_CREDS, &json)
}

#[tauri::command]
pub async fn forget_ghost_credentials() -> Result<(), String> {
    keychain::forget_slot(KEYRING_GHOST_CREDS)
}

#[tauri::command]
pub async fn set_wordpress_credentials(creds: WordpressCredentials) -> Result<(), String> {
    // Mirrors the Ghost path — see the rationale there. WordPress
    // Application Passwords ship as Basic auth, so plaintext is
    // even more catastrophic.
    rehydrate_publish::validate_remote_url(&creds.base_url).map_err(err)?;
    let json = serde_json::to_string(&creds).map_err(err)?;
    keychain::write_slot(KEYRING_WORDPRESS_CREDS, &json)
}

#[tauri::command]
pub async fn forget_wordpress_credentials() -> Result<(), String> {
    keychain::forget_slot(KEYRING_WORDPRESS_CREDS)
}

#[derive(Serialize)]
pub struct PublishCredentialStatus {
    pub ghost: bool,
    pub wordpress: bool,
}

#[tauri::command]
pub async fn publish_credential_status() -> Result<PublishCredentialStatus, String> {
    Ok(PublishCredentialStatus {
        ghost: keychain::read_slot(KEYRING_GHOST_CREDS).is_some(),
        wordpress: keychain::read_slot(KEYRING_WORDPRESS_CREDS).is_some(),
    })
}

/// Open a Ghost / WordPress draft URL in the user's default browser.
///
/// Sister command to `open_support_url`, but allowlisted dynamically
/// against the saved publish credentials rather than a hard-coded
/// prefix. The acceptance rules:
///
/// 1. `target` must have credentials in the keychain. No credentials
///    → no concept of a "trusted host for `target`" → refuse.
/// 2. The URL's host (ASCII-lowercase, via `host_of`) must match the
///    saved `base_url`'s host for that target. A renderer XSS that
///    only got hold of `target` and a forged URL can't pivot the
///    browser to an attacker-controlled domain — at worst it opens
///    a path on the user's own Ghost / WordPress site.
/// 3. The URL must parse and have a scheme of `http`/`https`.
///    `validate_remote_url` enforces this and the same SSRF guards
///    `set_ghost_credentials` already applies on the saved base URL.
///
/// Used by the Transcript drawer's "Draft created — View draft"
/// toast action so the user can actually open the post that was just
/// published. Before this, the `edit_url` was surfaced as plain text
/// in a transient toast and the user couldn't click it.
#[tauri::command]
pub async fn open_publish_url(
    url: String,
    target: PublishKind,
    app: AppHandle,
) -> Result<(), String> {
    rehydrate_publish::validate_remote_url(&url).map_err(err)?;

    let url_host = host_of(&url).ok_or_else(|| "could not parse host from URL".to_string())?;

    // Resolve the trusted host for `target` from saved credentials.
    // `load_*_client` returns "no credentials saved" — bubble that as a
    // distinct error so the renderer can prompt the user to configure
    // publishing instead of silently failing.
    let trusted_host = match target.target() {
        PublishTarget::Ghost => {
            let json = keychain::read_slot(KEYRING_GHOST_CREDS)
                .ok_or_else(|| "no Ghost credentials saved".to_string())?;
            let creds: GhostCredentials = serde_json::from_str(&json).map_err(err)?;
            host_of(&creds.base_url)
                .ok_or_else(|| "saved Ghost base URL has no host".to_string())?
        }
        PublishTarget::Wordpress => {
            let json = keychain::read_slot(KEYRING_WORDPRESS_CREDS)
                .ok_or_else(|| "no WordPress credentials saved".to_string())?;
            let creds: WordpressCredentials = serde_json::from_str(&json).map_err(err)?;
            host_of(&creds.base_url)
                .ok_or_else(|| "saved WordPress base URL has no host".to_string())?
        }
    };

    if url_host != trusted_host {
        let target_name = match target {
            PublishKind::Ghost => "Ghost",
            PublishKind::Wordpress => "WordPress",
        };
        return Err(format!(
            "refusing to open URL: host {url_host} does not match saved {target_name} host {trusted_host}",
        ));
    }

    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|e| format!("could not open {url}: {e}"))
}

#[tauri::command]
pub async fn ping_publish_target(target: PublishKind) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let publisher: Box<dyn Publisher> = match target.target() {
            PublishTarget::Ghost => Box::new(load_ghost_client()?),
            PublishTarget::Wordpress => Box::new(load_wordpress_client()?),
        };
        publisher.ping().map_err(err)
    })
    .await
    .map_err(err)?
}
