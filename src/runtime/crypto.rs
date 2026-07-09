//! Static encryption utilities for bot tokens using AES-256-GCM.

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::RngCore;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};

pub type Key = LessSafeKey;

/// Encrypts the token plaintext, prepends a 12-byte random nonce, and returns base64.
/// Formats output as base64: nonce(12) || ct || tag(16)
pub fn encrypt_token(plain: &str, key: &Key) -> Result<String> {
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);

    let mut in_out = plain.as_bytes().to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| anyhow::anyhow!("seal_in_place_append_tag failed"))?;

    let mut result = Vec::with_capacity(12 + in_out.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&in_out);

    Ok(B64.encode(&result))
}

/// Decrypts the token from base64 blob using the master key.
pub fn decrypt_token(blob: &str, key: &Key) -> Result<String> {
    let data = B64.decode(blob.trim())?;
    if data.len() < 12 + 16 {
        anyhow::bail!("Decryption failed: data too short");
    }
    let (nonce_bytes, ciphertext_tag) = data.split_at(12);

    let nonce = Nonce::assume_unique_for_key(
        nonce_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid nonce length"))?,
    );

    let mut in_out = ciphertext_tag.to_vec();
    let plaintext_bytes = key
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| anyhow::anyhow!("Decryption failed: bad key or corrupted data"))?;

    let plaintext = String::from_utf8(plaintext_bytes.to_vec())?;
    Ok(plaintext)
}

/// Load or derive the master key from the `ILINK_HUB_MASTER_KEY` environment variable.
/// The environment variable must be a 32-byte base64 or 32-byte hex string (64 characters).
pub fn load_or_derive_master_key() -> Result<Key> {
    let key_str = std::env::var("ILINK_HUB_MASTER_KEY")
        .map_err(|_| anyhow::anyhow!("ILINK_HUB_MASTER_KEY is required"))?;

    let key_bytes = decode_key(&key_str)?;

    let unbound_key = UnboundKey::new(&AES_256_GCM, &key_bytes)
        .map_err(|_| anyhow::anyhow!("Failed to create UnboundKey"))?;

    Ok(LessSafeKey::new(unbound_key))
}

fn decode_key(s: &str) -> Result<Vec<u8>> {
    let s = s.trim().trim_matches('"').trim_matches('\'');

    if let Ok(bytes) = B64.decode(s) {
        if bytes.len() == 32 {
            return Ok(bytes);
        }
    }

    if let Ok(bytes) = decode_hex(s) {
        if bytes.len() == 32 {
            return Ok(bytes);
        }
    }

    anyhow::bail!("Invalid key: must be 32 bytes in either base64 or hex format")
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        anyhow::bail!("Odd number of hexadecimal digits");
    }
    let mut res = Vec::with_capacity(s.len() / 2);
    for i in 0..(s.len() / 2) {
        let byte_str = &s[i * 2..i * 2 + 2];
        let byte = u8::from_str_radix(byte_str, 16)
            .map_err(|_| anyhow::anyhow!("Invalid hex character"))?;
        res.push(byte);
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(raw: [u8; 32]) -> Key {
        let unbound = UnboundKey::new(&AES_256_GCM, &raw).unwrap();
        LessSafeKey::new(unbound)
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = make_key([0u8; 32]);
        let token = "my-secret-bot-token-12345";
        let encrypted = encrypt_token(token, &key).unwrap();
        assert_ne!(token, encrypted);
        let decrypted = decrypt_token(&encrypted, &key).unwrap();
        assert_eq!(token, decrypted);
    }

    #[test]
    fn decrypt_token_too_short_blob_returns_error() {
        let key = make_key([0u8; 32]);
        // 20 bytes is < 12 (nonce) + 16 (tag) = 28 minimum
        let short_blob = B64.encode([0u8; 20]);
        let result = decrypt_token(&short_blob, &key);
        assert!(result.is_err(), "blob shorter than 28 bytes must fail");
    }

    /// Exactly 28 bytes must pass the length gate (`len < 28`), then fail at AEAD open.
    /// Catches ` < ` → ` <= ` (which would wrongly reject the minimum-length blob).
    #[test]
    fn decrypt_token_exactly_28_bytes_passes_length_check() {
        let key = make_key([0u8; 32]);
        let blob = B64.encode([0u8; 28]);
        let err = decrypt_token(&blob, &key)
            .expect_err("garbage 28-byte blob must still fail AEAD")
            .to_string();
        assert!(
            !err.contains("data too short"),
            "exactly 28 bytes must not trip the length check; got: {err}"
        );
    }

    #[test]
    fn decrypt_token_invalid_base64_returns_error() {
        let key = make_key([0u8; 32]);
        let result = decrypt_token("not-valid-base64!!", &key);
        assert!(result.is_err(), "invalid base64 must fail");
    }

    // ── decode_hex ────────────────────────────────────────────────────────────

    #[test]
    fn decode_hex_valid_even_length_input() {
        let bytes = decode_hex("0102ff").unwrap();
        assert_eq!(bytes, vec![0x01, 0x02, 0xff]);
    }

    /// Odd number of hex characters must be rejected.
    #[test]
    fn decode_hex_odd_length_returns_error() {
        let result = decode_hex("abc"); // 3 chars = odd
        assert!(result.is_err(), "odd-length hex must fail");
    }

    #[test]
    fn decode_hex_invalid_char_returns_error() {
        let result = decode_hex("zz");
        assert!(result.is_err(), "non-hex characters must fail");
    }

    #[test]
    fn decode_hex_empty_string_returns_empty_vec() {
        let bytes = decode_hex("").unwrap();
        assert!(
            bytes.is_empty(),
            "empty hex string must decode to empty vec"
        );
    }

    // ── decode_key ────────────────────────────────────────────────────────────

    #[test]
    fn decode_key_accepts_32_byte_base64() {
        let b64_key = B64.encode([42u8; 32]);
        let bytes = decode_key(&b64_key).unwrap();
        assert_eq!(bytes, vec![42u8; 32]);
    }

    #[test]
    fn decode_key_accepts_64_char_hex() {
        let hex_key = "00".repeat(32); // 32 bytes as hex = 64 chars
        let bytes = decode_key(&hex_key).unwrap();
        assert_eq!(bytes, vec![0u8; 32]);
    }

    #[test]
    fn decode_key_rejects_wrong_length_base64() {
        let b64_key = B64.encode([0u8; 16]); // only 16 bytes
        let result = decode_key(&b64_key);
        assert!(result.is_err(), "16-byte key must be rejected");
    }

    #[test]
    fn decode_key_strips_surrounding_quotes() {
        // Admins sometimes quote the env var value
        let raw = B64.encode([7u8; 32]);
        let quoted = format!("\"{raw}\"");
        let bytes = decode_key(&quoted).unwrap();
        assert_eq!(bytes, vec![7u8; 32]);
    }
}
