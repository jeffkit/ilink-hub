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
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
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

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let raw_key = [0u8; 32];
        let unbound_key = UnboundKey::new(&AES_256_GCM, &raw_key).unwrap();
        let key = LessSafeKey::new(unbound_key);

        let token = "my-secret-bot-token-12345";
        let encrypted = encrypt_token(token, &key).unwrap();
        assert_ne!(token, encrypted);

        let decrypted = decrypt_token(&encrypted, &key).unwrap();
        assert_eq!(token, decrypted);
    }
}
