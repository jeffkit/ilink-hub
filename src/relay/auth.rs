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

    /// M4: verify_register accepts timestamps within the skew window.
    /// Catches > → == and > → >= mutants on the skew check.
    #[test]
    fn verify_register_accepts_timestamp_within_skew_window() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let device_id = "test-device";
        let ts: i64 = 1_700_000_000;

        let sig = sign_register(&signing_key, device_id, ts);

        // now = ts + (REGISTER_MAX_SKEW_SECS - 1): just inside the window.
        let now_just_inside = ts + REGISTER_MAX_SKEW_SECS - 1;
        verify_register(&verifying_key, device_id, ts, &sig, now_just_inside)
            .expect("timestamp within skew window must be accepted");

        // now = ts - (REGISTER_MAX_SKEW_SECS - 1): future ts still within window.
        let now_just_inside_future = ts - (REGISTER_MAX_SKEW_SECS - 1);
        verify_register(&verifying_key, device_id, ts, &sig, now_just_inside_future)
            .expect("future timestamp within skew window must be accepted");
    }

    /// M4: verify_register rejects timestamps outside the skew window.
    /// Catches - → / mutant (time-diff calculation) and > → == / >= mutants.
    #[test]
    fn verify_register_rejects_timestamp_outside_skew_window() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let device_id = "test-device";
        let ts: i64 = 1_700_000_000;
        let sig = sign_register(&signing_key, device_id, ts);

        // now = ts + REGISTER_MAX_SKEW_SECS + 1: just past the deadline.
        let now_expired = ts + REGISTER_MAX_SKEW_SECS + 1;
        assert!(
            verify_register(&verifying_key, device_id, ts, &sig, now_expired).is_err(),
            "timestamp past skew window must be rejected"
        );

        // now = ts - (REGISTER_MAX_SKEW_SECS + 1): far future ts.
        let now_future_expired = ts - (REGISTER_MAX_SKEW_SECS + 1);
        assert!(
            verify_register(&verifying_key, device_id, ts, &sig, now_future_expired).is_err(),
            "far-future timestamp must be rejected"
        );
    }

    /// M4: public_key_b64 and verifying_key_from_b64 form a round-trip.
    /// Catches the public_key_b64 → String::new() / "xyzzy" mutants and the
    /// verifying_key_from_b64 → Ok(Default::default()) mutant.
    #[test]
    fn public_key_b64_and_verifying_key_from_b64_round_trip() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let original_vk = signing_key.verifying_key();

        let encoded = public_key_b64(&original_vk);
        assert!(
            !encoded.is_empty(),
            "public_key_b64 must produce a non-empty string"
        );

        let decoded_vk =
            verifying_key_from_b64(&encoded).expect("round-trip must decode successfully");
        assert_eq!(
            original_vk.to_bytes(),
            decoded_vk.to_bytes(),
            "decoded verifying key must equal the original"
        );
    }
}
