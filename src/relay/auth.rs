//! Ed25519 registration signatures for pairing relay.

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

pub const REGISTER_MAX_SKEW_SECS: i64 = 60;

pub fn register_payload(device_id: &str, timestamp: i64) -> String {
    format!("register:{device_id}:{timestamp}")
}

pub fn sign_register(signing_key: &SigningKey, device_id: &str, timestamp: i64) -> String {
    let payload = register_payload(device_id, timestamp);
    let sig = signing_key.sign(payload.as_bytes());
    B64.encode(sig.to_bytes())
}

pub fn verify_register(
    verifying_key: &VerifyingKey,
    device_id: &str,
    timestamp: i64,
    signature_b64: &str,
    now_unix: i64,
) -> Result<()> {
    if (now_unix - timestamp).abs() > REGISTER_MAX_SKEW_SECS {
        return Err(anyhow!("registration timestamp out of range"));
    }

    let sig_bytes = B64
        .decode(signature_b64)
        .context("invalid signature encoding")?;
    let signature = Signature::from_slice(&sig_bytes).context("invalid signature length")?;

    let payload = register_payload(device_id, timestamp);
    verifying_key
        .verify(payload.as_bytes(), &signature)
        .map_err(|_| anyhow!("invalid registration signature"))
}

pub fn verifying_key_from_b64(public_key_b64: &str) -> Result<VerifyingKey> {
    let bytes = B64
        .decode(public_key_b64)
        .context("invalid public_key encoding")?;
    VerifyingKey::from_bytes(
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("public_key must be 32 bytes"))?,
    )
    .context("invalid public_key")
}

pub fn public_key_b64(verifying_key: &VerifyingKey) -> String {
    B64.encode(verifying_key.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;

    #[test]
    fn sign_and_verify_register() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let device_id = "550e8400-e29b-41d4-a716-446655440000";
        let ts = 1_700_000_000;
        let sig = sign_register(&signing_key, device_id, ts);
        verify_register(&verifying_key, device_id, ts, &sig, ts).unwrap();
    }

    #[test]
    fn rejects_wrong_device_id() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let sig = sign_register(&signing_key, "device-a", 100);
        assert!(verify_register(&verifying_key, "device-b", 100, &sig, 100).is_err());
    }
}
