//! Envelope encryption for sync secrets (ADR-0001).
//!
//! Uses AES-256-GCM to encrypt per-user encryption secrets with the server's
//! master key. The master key is provided via `CMDOCK_MASTER_KEY` env var.
//!
//! Wire format: nonce (12 bytes) + ciphertext + tag (16 bytes)

use ring::aead;
use ring::rand::{SecureRandom, SystemRandom};

/// Encrypt a plaintext secret with the master key (AES-256-GCM).
///
/// `master_key` must be exactly 32 bytes.
/// Returns: nonce (12) + ciphertext + tag (16).
pub fn encrypt_secret(plaintext: &[u8], master_key: &[u8]) -> anyhow::Result<Vec<u8>> {
    if master_key.len() != 32 {
        anyhow::bail!(
            "master key must be exactly 32 bytes, got {}",
            master_key.len()
        );
    }
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, master_key)
        .map_err(|_| anyhow::anyhow!("failed to create AES-256-GCM key"))?;
    let key = aead::LessSafeKey::new(key);
    let rng = SystemRandom::new();
    let mut nonce_bytes = [0u8; 12];
    rng.fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("failed to generate random nonce"))?;
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let mut ciphertext = plaintext.to_vec();
    let tag = key
        .seal_in_place_separate_tag(nonce, aead::Aad::empty(), &mut ciphertext)
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    // Format: nonce (12) + ciphertext + tag (16)
    let mut result = Vec::with_capacity(12 + ciphertext.len() + 16);
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    result.extend_from_slice(tag.as_ref());
    Ok(result)
}

/// Decrypt a secret encrypted with [`encrypt_secret`].
///
/// `master_key` must be exactly 32 bytes.
pub fn decrypt_secret(encrypted: &[u8], master_key: &[u8]) -> anyhow::Result<Vec<u8>> {
    if master_key.len() != 32 {
        anyhow::bail!(
            "master key must be exactly 32 bytes, got {}",
            master_key.len()
        );
    }
    if encrypted.len() < 12 + 16 {
        anyhow::bail!(
            "encrypted data too short ({} bytes, minimum 28)",
            encrypted.len()
        );
    }
    let key = aead::UnboundKey::new(&aead::AES_256_GCM, master_key)
        .map_err(|_| anyhow::anyhow!("failed to create AES-256-GCM key"))?;
    let key = aead::LessSafeKey::new(key);
    let nonce = aead::Nonce::assume_unique_for_key(
        encrypted[..12]
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid nonce"))?,
    );
    let mut data = encrypted[12..].to_vec();
    let plaintext = key
        .open_in_place(nonce, aead::Aad::empty(), &mut data)
        .map_err(|_| anyhow::anyhow!("decryption failed (wrong key or tampered data)"))?;
    Ok(plaintext.to_vec())
}

/// Derive a per-device encryption secret from the user's master secret using HKDF.
///
/// Each device gets a unique, deterministic secret derived from:
/// - `master_secret`: the user's raw encryption secret (from replicas table, decrypted)
/// - `client_id`: the device's UUID (used as HKDF salt for domain separation)
///
/// Returns 32 raw bytes suitable for hex-encoding and passing to the TW CLI.
/// The derivation is deterministic — same inputs always produce the same output.
pub fn derive_device_secret(master_secret: &[u8], client_id: &[u8]) -> anyhow::Result<Vec<u8>> {
    let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, client_id);
    let prk = salt.extract(master_secret);
    let info = [b"cmdock-device-v1".as_slice()];
    let okm = prk
        .expand(&info, HkdfLen(32))
        .map_err(|_| anyhow::anyhow!("HKDF expand failed"))?;
    let mut out = vec![0u8; 32];
    okm.fill(&mut out)
        .map_err(|_| anyhow::anyhow!("HKDF fill failed"))?;
    Ok(out)
}

/// Helper for ring's HKDF: tells it how many bytes we want.
struct HkdfLen(usize);

impl ring::hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        self.0
    }
}

/// Parse a master key from hex or base64 string. Returns 32 bytes.
pub fn parse_master_key(input: &str) -> anyhow::Result<[u8; 32]> {
    let input = input.trim();
    // Try hex first (64 hex chars = 32 bytes)
    if input.len() == 64 && input.chars().all(|c| c.is_ascii_hexdigit()) {
        let bytes = hex::decode(input)?;
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    // Try base64 (44 chars with padding = 32 bytes)
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, input)
        .or_else(|_| base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE, input))
        .map_err(|_| {
            anyhow::anyhow!("master key must be 64 hex chars or 44 base64 chars (32 bytes)")
        })?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "master key must decode to exactly 32 bytes, got {}",
            bytes.len()
        );
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = i as u8;
        }
        key
    }

    #[test]
    fn test_encrypt_decrypt_round_trip() {
        let key = test_key();
        let plaintext = b"my-encryption-secret-for-tc-sync";
        let encrypted = encrypt_secret(plaintext, &key).unwrap();
        assert_ne!(
            &encrypted, plaintext,
            "ciphertext should differ from plaintext"
        );
        assert!(encrypted.len() >= 12 + plaintext.len() + 16);
        let decrypted = decrypt_secret(&encrypted, &key).unwrap();
        assert_eq!(&decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_empty_plaintext() {
        let key = test_key();
        let plaintext = b"";
        let encrypted = encrypt_secret(plaintext, &key).unwrap();
        let decrypted = decrypt_secret(&encrypted, &key).unwrap();
        assert_eq!(&decrypted, plaintext);
    }

    #[test]
    fn test_wrong_key_fails() {
        let key1 = test_key();
        let mut key2 = test_key();
        key2[0] = 0xFF; // different key
        let plaintext = b"secret-data";
        let encrypted = encrypt_secret(plaintext, &key1).unwrap();
        let result = decrypt_secret(&encrypted, &key2);
        assert!(result.is_err(), "decryption with wrong key should fail");
    }

    #[test]
    fn test_tampered_data_fails() {
        let key = test_key();
        let plaintext = b"secret-data";
        let mut encrypted = encrypt_secret(plaintext, &key).unwrap();
        // Flip a byte in the ciphertext area
        let mid = encrypted.len() / 2;
        encrypted[mid] ^= 0xFF;
        let result = decrypt_secret(&encrypted, &key);
        assert!(result.is_err(), "decryption of tampered data should fail");
    }

    #[test]
    fn test_data_too_short() {
        let key = test_key();
        let result = decrypt_secret(&[0u8; 10], &key);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_key_length() {
        let short_key = [0u8; 16];
        let result = encrypt_secret(b"data", &short_key);
        assert!(result.is_err());
        let result = decrypt_secret(&[0u8; 30], &short_key);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_master_key_hex() {
        let hex_key = "0001020304050607080910111213141516171819202122232425262728293031";
        let result = parse_master_key(hex_key);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 32);
    }

    #[test]
    fn test_parse_master_key_base64() {
        // 32 bytes in base64
        let key_bytes = test_key();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key_bytes);
        let result = parse_master_key(&b64);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), key_bytes);
    }

    #[test]
    fn test_parse_master_key_invalid() {
        assert!(parse_master_key("too-short").is_err());
        assert!(parse_master_key("").is_err());
    }

    #[test]
    fn test_derive_device_secret_deterministic() {
        let master = b"master-encryption-secret-32bytes!";
        let client_id = b"c0c173fd-706d-4c9b-aed2-ca6dde18347c";
        let s1 = derive_device_secret(master, client_id).unwrap();
        let s2 = derive_device_secret(master, client_id).unwrap();
        assert_eq!(s1, s2, "same inputs must produce same output");
        assert_eq!(s1.len(), 32);
    }

    #[test]
    fn test_derive_device_secret_different_devices() {
        let master = b"master-encryption-secret-32bytes!";
        let s1 = derive_device_secret(master, b"device-aaa").unwrap();
        let s2 = derive_device_secret(master, b"device-bbb").unwrap();
        assert_ne!(
            s1, s2,
            "different client_ids must produce different secrets"
        );
    }

    #[test]
    fn test_derive_device_secret_different_masters() {
        let client_id = b"same-device-id";
        let s1 = derive_device_secret(b"master-one", client_id).unwrap();
        let s2 = derive_device_secret(b"master-two", client_id).unwrap();
        assert_ne!(
            s1, s2,
            "different master secrets must produce different outputs"
        );
    }
}
