//! TC-compatible encryption envelope for the sync protocol.
//!
//! Reimplements the TaskChampion encryption format so our server can encrypt
//! and decrypt history segments and snapshots on behalf of a user whose
//! encryption secret we escrow.
//!
//! Wire format (identical to TC):
//!   `[1-byte version] + [12-byte nonce] + [ciphertext + 16-byte Poly1305 tag]`
//!
//! Key derivation: PBKDF2-HMAC-SHA256 with 600,000 iterations.
//! Cipher: ChaCha20-Poly1305 (IETF).
//! AAD: `[1-byte app_id=1] + [16-byte version_id]`.

use ring::{aead, pbkdf2, rand, rand::SecureRandom};
use uuid::Uuid;

const PBKDF2_ITERATIONS: u32 = 600_000;
const ENVELOPE_VERSION: u8 = 1;
const TASK_APP_ID: u8 = 1;

/// TC sync protocol encryptor/decryptor.
///
/// Derives a ChaCha20-Poly1305 key from `encryption_secret` + `client_id` (salt).
/// The derived key is cached, so create one per user/client_id and reuse.
pub struct SyncCryptor {
    key: aead::LessSafeKey,
    rng: rand::SystemRandom,
}

impl SyncCryptor {
    /// Create a new cryptor with the given `client_id` (used as PBKDF2 salt) and
    /// encryption secret.
    ///
    /// Key derivation takes ~10 ms due to PBKDF2 iterations.
    pub fn new(client_id: Uuid, encryption_secret: &[u8]) -> anyhow::Result<Self> {
        let mut key_bytes = vec![0u8; aead::CHACHA20_POLY1305.key_len()];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            std::num::NonZeroU32::new(PBKDF2_ITERATIONS).unwrap(),
            client_id.as_bytes(),
            encryption_secret,
            &mut key_bytes,
        );
        let unbound = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key_bytes)
            .map_err(|e| anyhow::anyhow!("AEAD key error: {e}"))?;
        Ok(Self {
            key: aead::LessSafeKey::new(unbound),
            rng: rand::SystemRandom::new(),
        })
    }

    /// Encrypt (seal) a plaintext payload for a given `version_id`.
    ///
    /// Returns the envelope: `[version_byte] + [nonce] + [ciphertext + tag]`.
    pub fn seal(&self, version_id: Uuid, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut nonce_bytes = [0u8; aead::NONCE_LEN]; // 12 bytes
        self.rng
            .fill(&mut nonce_bytes)
            .map_err(|e| anyhow::anyhow!("RNG error: {e}"))?;
        let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);

        let aad = Self::make_aad(version_id);
        let mut data = plaintext.to_vec();
        let tag = self
            .key
            .seal_in_place_separate_tag(nonce, aad, &mut data)
            .map_err(|e| anyhow::anyhow!("seal error: {e}"))?;
        data.extend_from_slice(tag.as_ref());

        // Envelope: [1-byte version] + [12-byte nonce] + [ciphertext + tag]
        let mut envelope = Vec::with_capacity(1 + aead::NONCE_LEN + data.len());
        envelope.push(ENVELOPE_VERSION);
        envelope.extend_from_slice(&nonce_bytes);
        envelope.extend_from_slice(&data);
        Ok(envelope)
    }

    /// Decrypt (unseal) an envelope, verifying it was created for the given `version_id`.
    pub fn unseal(&self, version_id: Uuid, envelope: &[u8]) -> anyhow::Result<Vec<u8>> {
        if envelope.len() <= 1 + aead::NONCE_LEN {
            anyhow::bail!("envelope too short");
        }
        if envelope[0] != ENVELOPE_VERSION {
            anyhow::bail!("unrecognised envelope version {}", envelope[0]);
        }

        let nonce_bytes: [u8; 12] = envelope[1..13]
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid nonce slice"))?;
        let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
        let aad = Self::make_aad(version_id);

        let mut data = envelope[13..].to_vec();
        let plaintext = self
            .key
            .open_in_place(nonce, aad, &mut data)
            .map_err(|e| anyhow::anyhow!("unseal error: {e}"))?;
        Ok(plaintext.to_vec())
    }

    fn make_aad(version_id: Uuid) -> aead::Aad<[u8; 17]> {
        let mut aad = [0u8; 17];
        aad[0] = TASK_APP_ID;
        aad[1..].copy_from_slice(version_id.as_bytes());
        aead::Aad::from(aad)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cryptor() -> SyncCryptor {
        let client_id = Uuid::new_v4();
        SyncCryptor::new(client_id, b"test-secret").unwrap()
    }

    #[test]
    fn test_seal_unseal_round_trip() {
        let cryptor = test_cryptor();
        let version_id = Uuid::new_v4();
        let plaintext = b"hello world, this is a test payload";

        let envelope = cryptor.seal(version_id, plaintext).unwrap();
        let decrypted = cryptor.unseal(version_id, &envelope).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_wrong_key_fails() {
        let client_id = Uuid::new_v4();
        let cryptor1 = SyncCryptor::new(client_id, b"secret-one").unwrap();
        let cryptor2 = SyncCryptor::new(client_id, b"secret-two").unwrap();
        let version_id = Uuid::new_v4();

        let envelope = cryptor1.seal(version_id, b"payload").unwrap();
        let result = cryptor2.unseal(version_id, &envelope);
        assert!(result.is_err(), "decryption with wrong key should fail");
    }

    #[test]
    fn test_wrong_version_id_fails() {
        let cryptor = test_cryptor();
        let v1 = Uuid::new_v4();
        let v2 = Uuid::new_v4();

        let envelope = cryptor.seal(v1, b"payload").unwrap();
        let result = cryptor.unseal(v2, &envelope);
        assert!(result.is_err(), "unseal with wrong version_id should fail");
    }

    #[test]
    fn test_empty_payload() {
        let cryptor = test_cryptor();
        let version_id = Uuid::new_v4();

        let envelope = cryptor.seal(version_id, b"").unwrap();
        let decrypted = cryptor.unseal(version_id, &envelope).unwrap();
        assert!(decrypted.is_empty());
    }

    #[test]
    fn test_cross_validate_with_tc_test_vector() {
        // These are the exact values from TC's generate-test-data.py
        let version_id = Uuid::parse_str("b0517957-f912-4d49-8330-f612e73030c4").unwrap();
        let encryption_secret = b"b4a4e6b7b811eda1dc1a2693ded";
        let client_id = Uuid::parse_str("0666d464-418a-4a08-ad53-6f15c78270cd").unwrap();

        let test_data = include_bytes!("test-good.data");

        let cryptor = SyncCryptor::new(client_id, encryption_secret).unwrap();
        let plaintext = cryptor.unseal(version_id, test_data).unwrap();
        assert_eq!(plaintext, b"SUCCESS");
    }
}
