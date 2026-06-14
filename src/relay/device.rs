//! Persistent device identity for zero-config pairing relay.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

use super::auth::{public_key_b64, sign_register};

const DEVICE_ID_FILE: &str = "device_id";
const DEVICE_IDENTITY_FILE: &str = "device_identity.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub device_id: String,
    #[serde(rename = "signing_key")]
    signing_key_b64: String,
}

impl DeviceIdentity {
    pub fn load_or_create() -> Result<Self> {
        let path = device_identity_path()?;
        if path.exists() {
            let raw =
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            let id: Self = serde_json::from_str(&raw).context("parse device_identity.json")?;
            if validate_device_id(&id.device_id) && !id.signing_key_b64.is_empty() {
                return Ok(id);
            }
            tracing::warn!("invalid device_identity.json, regenerating");
        }

        // Migrate legacy device_id file if present.
        let legacy_path = device_id_path()?;
        let device_id = if legacy_path.exists() {
            let id = fs::read_to_string(&legacy_path)?.trim().to_string();
            if validate_device_id(&id) {
                id
            } else {
                Uuid::new_v4().to_string()
            }
        } else {
            Uuid::new_v4().to_string()
        };

        let signing_key = SigningKey::generate(&mut OsRng);
        let identity = Self {
            device_id,
            signing_key_b64: B64.encode(signing_key.to_bytes()),
        };
        identity.save()?;
        tracing::info!(
            device_id = %identity.device_id,
            path = %path.display(),
            "created device identity"
        );
        Ok(identity)
    }

    fn save(&self) -> Result<()> {
        let path = device_identity_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("write {}", path.display()))?;
        // Restrict to owner-read/write only — the file contains the Ed25519 signing key.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 0600 {}", path.display()))?;
        }
        Ok(())
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    fn signing_key(&self) -> Result<SigningKey> {
        let bytes = B64
            .decode(&self.signing_key_b64)
            .context("decode signing_key")?;
        Ok(SigningKey::from_bytes(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("signing_key must be 32 bytes"))?,
        ))
    }

    pub fn verifying_key(&self) -> Result<VerifyingKey> {
        Ok(self.signing_key()?.verifying_key())
    }

    pub fn public_key_b64(&self) -> Result<String> {
        Ok(public_key_b64(&self.verifying_key()?))
    }

    pub fn sign_register(&self, timestamp: i64) -> Result<String> {
        Ok(sign_register(
            &self.signing_key()?,
            &self.device_id,
            timestamp,
        ))
    }

    /// Build an identity from already-known material. Used by tests so they do
    /// not need to write to the user's real local data dir.
    #[doc(hidden)]
    pub fn for_testing(device_id: String, signing_key_b64: String) -> Self {
        Self {
            device_id,
            signing_key_b64,
        }
    }
}

/// Backward-compatible helper for code that only needs the device id string.
pub fn load_or_create_device_id() -> Result<String> {
    Ok(DeviceIdentity::load_or_create()?.device_id)
}

pub fn device_identity_path() -> Result<PathBuf> {
    let base = dirs::data_local_dir().context("could not resolve data local dir")?;
    Ok(base.join("ilink-hub").join(DEVICE_IDENTITY_FILE))
}

pub fn device_id_path() -> Result<PathBuf> {
    let base = dirs::data_local_dir().context("could not resolve data local dir")?;
    Ok(base.join("ilink-hub").join(DEVICE_ID_FILE))
}

pub fn validate_device_id(id: &str) -> bool {
    if id.len() < 8 || id.len() > 64 {
        return false;
    }
    id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Only pairing endpoints may be forwarded from the public relay.
///
/// Rejects any path that could trick reqwest into routing the request
/// somewhere other than `/hub/pair/...` on the local hub:
///   * network-path references (`//evil.example.com/...`) — reqwest would
///     treat `//authority` as the host and rewrite the destination.
///   * query strings (`?...`) and fragments (`#...`) — relay-supplied
///     noise that the local hub has no contract for.
///   * path traversal (`..`) — belt-and-braces, the forwarded path is
///     appended verbatim to `hub_base`.
///   * anything that does not start with the single `/hub/pair/` prefix.
pub fn is_allowed_relay_path(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    // Network-path reference: a second `/` immediately after the leading
    // one would make reqwest parse the URL with `path` as authority.
    if path.starts_with("//") {
        return false;
    }
    if path.contains("..") || path.contains('?') || path.contains('#') {
        return false;
    }
    path.starts_with("/hub/pair/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_device_id() {
        assert!(validate_device_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!validate_device_id("bad id"));
        assert!(!validate_device_id("x"));
    }

    #[test]
    fn relay_path_whitelist() {
        assert!(is_allowed_relay_path("/hub/pair/pair_abc"));
        assert!(is_allowed_relay_path("/hub/pair/pair_abc/confirm"));
        assert!(!is_allowed_relay_path("/hub/clients"));
        assert!(!is_allowed_relay_path("/hub/pair/../admin"));
    }

    /// Adversarial coverage for F-2: paths that could trick reqwest into
    /// routing the request somewhere other than `/hub/pair/...` on the
    /// local hub. Each case must be rejected.
    #[test]
    fn relay_path_whitelist_rejects_network_path_and_query_and_fragment() {
        // Network-path reference: reqwest parses `//authority/path` as
        // scheme-relative with `authority` as the host.
        assert!(!is_allowed_relay_path("//evil.example.com/hub/pair/x"));
        assert!(!is_allowed_relay_path("//127.0.0.1:9999/hub/pair/x"));

        // Query and fragment.
        assert!(!is_allowed_relay_path("/hub/pair/abc?forward=evil"));
        assert!(!is_allowed_relay_path("/hub/pair/abc#fragment"));

        // Path traversal (already covered, kept here for completeness).
        assert!(!is_allowed_relay_path("/hub/pair/../admin"));

        // Must start with `/`.
        assert!(!is_allowed_relay_path("hub/pair/abc"));
        assert!(!is_allowed_relay_path(""));
        assert!(!is_allowed_relay_path("/"));
    }
}
