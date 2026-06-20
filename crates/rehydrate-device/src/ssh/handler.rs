use std::sync::Arc;

use russh::client::Handler;
use russh::keys::PublicKey;

use crate::known_hosts::KnownHosts;

/// russh client handler doing trust-on-first-use host-key pinning.
///
/// On first connect to a given `host:port` the server's SHA256
/// fingerprint is pinned. Every subsequent connect compares the
/// presented key to the pinned value and refuses to proceed when they
/// don't match — propagating the mismatch back to `SshDevice::connect`
/// through the shared `outcome` slot, which is then surfaced as
/// [`crate::error::DeviceError::HostKeyChanged`].
///
/// The outcome is held in `Arc<std::sync::Mutex<_>>` rather than
/// directly on the handler because russh's `client::connect` consumes
/// the handler by value; sharing the slot lets the caller read the
/// verification result after the handshake completes.
pub(super) struct ClientHandler {
    pub(super) known_hosts: KnownHosts,
    pub(super) endpoint: String,
    pub(super) outcome: Arc<std::sync::Mutex<HostKeyOutcome>>,
}

#[derive(Debug, Clone, Default)]
pub(super) enum HostKeyOutcome {
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
