//! Crate-level KMS encrypt/decrypt API for cross-service callers.
//!
//! Real AES-256-GCM with a fresh 12-byte IV per call and an
//! authenticated tag. Envelope format:
//!
//! ```text
//! | key_arn_len:u16_be | key_arn_utf8 | iv:12 | ciphertext_with_tag |
//! ```
//!
//! The key ARN is embedded so decryption callers can pass opaque
//! ciphertext back without tracking the key separately — matching how
//! AWS's KMS blob format self-describes.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use sha2::{Digest, Sha256};

use crate::state::{KmsKey, SharedKmsState};

#[derive(Debug, thiserror::Error)]
pub enum KmsApiError {
    #[error("KMS key {0} not found")]
    KeyNotFound(String),
    #[error("KMS key {0} is not enabled")]
    KeyDisabled(String),
    #[error("encryption failed: {0}")]
    EncryptFailed(String),
    #[error("decryption failed: {0}")]
    DecryptFailed(String),
    #[error("malformed ciphertext envelope")]
    MalformedCiphertext,
}

/// Encrypt `plaintext` under the AES-256 key derived from `key_ref`
/// (key id, key ARN, alias name or alias ARN). An alias name resolves in
/// `region`, the region of the resource being encrypted; an alias ARN in the
/// region it names. Returns an envelope that `decrypt_blob` accepts without
/// needing the key-ref passed again.
pub fn encrypt_blob(
    state: &SharedKmsState,
    account_id: &str,
    region: &str,
    key_ref: &str,
    plaintext: &[u8],
) -> Result<Vec<u8>, KmsApiError> {
    let (key_arn, aes_key) = {
        let accounts = state.read();
        let s = accounts
            .get(account_id)
            .ok_or_else(|| KmsApiError::KeyNotFound(key_ref.to_string()))?;
        let key = lookup_key(s, region, key_ref)
            .ok_or_else(|| KmsApiError::KeyNotFound(key_ref.to_string()))?;
        if !key.enabled {
            return Err(KmsApiError::KeyDisabled(key.key_id.clone()));
        }
        (key.arn.clone(), derive_aes_key(key))
    };

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&aes_key));
    let iv = random_iv();
    let nonce = Nonce::from_slice(&iv);
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad: key_arn.as_bytes(),
            },
        )
        .map_err(|e| KmsApiError::EncryptFailed(e.to_string()))?;

    let arn_bytes = key_arn.as_bytes();
    let arn_len = arn_bytes.len() as u16;
    let mut out = Vec::with_capacity(2 + arn_bytes.len() + 12 + ciphertext.len());
    out.extend_from_slice(&arn_len.to_be_bytes());
    out.extend_from_slice(arn_bytes);
    out.extend_from_slice(&iv);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt a blob produced by `encrypt_blob`.
pub fn decrypt_blob(
    state: &SharedKmsState,
    account_id: &str,
    ciphertext: &[u8],
) -> Result<Vec<u8>, KmsApiError> {
    if ciphertext.len() < 2 {
        return Err(KmsApiError::MalformedCiphertext);
    }
    let arn_len = u16::from_be_bytes([ciphertext[0], ciphertext[1]]) as usize;
    let header_end = 2 + arn_len;
    if ciphertext.len() < header_end + 12 + 16 {
        return Err(KmsApiError::MalformedCiphertext);
    }
    let key_arn = std::str::from_utf8(&ciphertext[2..header_end])
        .map_err(|_| KmsApiError::MalformedCiphertext)?;
    let iv = &ciphertext[header_end..header_end + 12];
    let body = &ciphertext[header_end + 12..];

    let aes_key = {
        let accounts = state.read();
        let s = accounts
            .get(account_id)
            .ok_or_else(|| KmsApiError::KeyNotFound(key_arn.to_string()))?;
        // The envelope carries a key ARN, which resolves on its own.
        let key = lookup_key(s, "", key_arn)
            .ok_or_else(|| KmsApiError::KeyNotFound(key_arn.to_string()))?;
        if !key.enabled {
            return Err(KmsApiError::KeyDisabled(key.key_id.clone()));
        }
        derive_aes_key(key)
    };

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&aes_key));
    let nonce = Nonce::from_slice(iv);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: body,
                aad: key_arn.as_bytes(),
            },
        )
        .map_err(|e| KmsApiError::DecryptFailed(e.to_string()))
}

/// Resolve `key_ref` against a KMS state: a key id, key ARN, alias name
/// (resolved in `region`) or alias ARN (resolved in the region it names).
/// Returns the canonical key.
fn lookup_key<'a>(
    s: &'a crate::state::KmsState,
    region: &str,
    key_ref: &str,
) -> Option<&'a KmsKey> {
    let id = crate::service::KmsService::resolve_key_id_with_state(s, region, key_ref)?;
    s.keys.get(&id)
}

/// Derive a stable 32-byte AES key from a KmsKey. Priority:
/// `imported_material_bytes` (when caller used `ImportKeyMaterial`),
/// else the `private_key_seed` which every CreateKey populates.
/// Hashed with SHA-256 so we always end up with the right length
/// regardless of the source length.
fn derive_aes_key(key: &KmsKey) -> [u8; 32] {
    let source: &[u8] = key
        .imported_material_bytes
        .as_deref()
        .unwrap_or(&key.private_key_seed);
    let mut hasher = Sha256::new();
    hasher.update(b"fakecloud-kms-aes256:");
    hasher.update(key.key_id.as_bytes());
    hasher.update(b":");
    hasher.update(source);
    let out = hasher.finalize();
    let mut aes_key = [0u8; 32];
    aes_key.copy_from_slice(&out[..]);
    aes_key
}

fn random_iv() -> [u8; 12] {
    // aes-gcm re-exports an rng trait; keep a small local RNG using
    // timestamp + key-independent entropy. CI fakes don't need
    // cryptographic-quality randomness, but each IV must be unique
    // per key+plaintext pair, so mix in a monotonic counter.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let cnt = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(ts.to_be_bytes());
    hasher.update(cnt.to_be_bytes());
    hasher.update(std::process::id().to_be_bytes());
    let digest = hasher.finalize();
    let mut iv = [0u8; 12];
    iv.copy_from_slice(&digest[..12]);
    iv
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use fakecloud_aws::arn::Arn;
    use fakecloud_core::multi_account::MultiAccountState;
    use parking_lot::RwLock;

    use super::*;
    use crate::state::{KmsKey, KmsState};

    fn make_state_with_key() -> (SharedKmsState, String) {
        let state = Arc::new(RwLock::new(MultiAccountState::<KmsState>::new(
            "123456789012",
            "us-east-1",
            "http://localhost:4566",
        )));
        let key_id = "00000000-0000-0000-0000-000000000001".to_string();
        let arn =
            Arn::new("kms", "us-east-1", "123456789012", &format!("key/{key_id}")).to_string();
        {
            let mut accounts = state.write();
            let s = accounts.get_or_create("123456789012");
            s.keys.insert(
                key_id.clone(),
                KmsKey {
                    key_id: key_id.clone(),
                    arn: arn.clone(),
                    creation_date: 0.0,
                    description: String::new(),
                    enabled: true,
                    key_usage: "ENCRYPT_DECRYPT".into(),
                    key_spec: "SYMMETRIC_DEFAULT".into(),
                    key_manager: "CUSTOMER".into(),
                    key_state: "Enabled".into(),
                    deletion_date: None,
                    tags: Default::default(),
                    policy: String::new(),
                    key_rotation_enabled: false,
                    rotation_period_in_days: None,
                    origin: "AWS_KMS".into(),
                    multi_region: false,
                    rotations: Vec::new(),
                    signing_algorithms: None,
                    encryption_algorithms: None,
                    mac_algorithms: None,
                    custom_key_store_id: None,
                    imported_key_material: false,
                    imported_material_bytes: None,
                    private_key_seed: vec![7; 32],
                    primary_region: None,
                    asymmetric_private_key_der: None,
                    asymmetric_public_key_der: None,
                },
            );
        }
        (state, arn)
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let (state, arn) = make_state_with_key();
        let plaintext = b"hello fakecloud kms";
        let ct = encrypt_blob(&state, "123456789012", "us-east-1", &arn, plaintext).unwrap();
        assert_ne!(&ct[..], plaintext, "ciphertext must differ from plaintext");
        let pt = decrypt_blob(&state, "123456789012", &ct).unwrap();
        assert_eq!(pt.as_slice(), plaintext);
    }

    #[test]
    fn each_encrypt_yields_distinct_ciphertext() {
        let (state, arn) = make_state_with_key();
        let a = encrypt_blob(&state, "123456789012", "us-east-1", &arn, b"same plaintext").unwrap();
        let b = encrypt_blob(&state, "123456789012", "us-east-1", &arn, b"same plaintext").unwrap();
        assert_ne!(a, b, "distinct IVs should produce distinct ciphertext");
    }

    #[test]
    fn decrypt_with_tampered_ciphertext_fails() {
        let (state, arn) = make_state_with_key();
        let mut ct = encrypt_blob(&state, "123456789012", "us-east-1", &arn, b"tamper me").unwrap();
        // Flip a bit in the payload region.
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        assert!(decrypt_blob(&state, "123456789012", &ct).is_err());
    }

    #[test]
    fn decrypt_with_disabled_key_fails() {
        let (state, arn) = make_state_with_key();
        let ct = encrypt_blob(&state, "123456789012", "us-east-1", &arn, b"ok").unwrap();
        {
            let mut accounts = state.write();
            let s = accounts.get_mut("123456789012").unwrap();
            for k in s.keys.values_mut() {
                k.enabled = false;
            }
        }
        assert!(matches!(
            decrypt_blob(&state, "123456789012", &ct),
            Err(KmsApiError::KeyDisabled(_))
        ));
    }

    /// An alias name resolves in the region passed (the encrypted resource's
    /// region); an alias ARN in the region it names.
    #[test]
    fn alias_references_resolve_regionally() {
        let (state, arn) = make_state_with_key();
        let key_id = arn.rsplit('/').next().unwrap().to_string();
        {
            let mut accounts = state.write();
            let s = accounts.get_or_create("123456789012");
            s.insert_alias(
                "us-east-1",
                crate::state::KmsAlias {
                    alias_name: "alias/layers".into(),
                    alias_arn: crate::state::kms_alias_arn(
                        "us-east-1",
                        "123456789012",
                        "alias/layers",
                    ),
                    target_key_id: key_id,
                    creation_date: 0.0,
                },
            );
        }
        let ct = encrypt_blob(&state, "123456789012", "us-east-1", "alias/layers", b"x").unwrap();
        assert_eq!(decrypt_blob(&state, "123456789012", &ct).unwrap(), b"x");
        assert!(matches!(
            encrypt_blob(&state, "123456789012", "eu-west-1", "alias/layers", b"x"),
            Err(KmsApiError::KeyNotFound(_))
        ));
        let alias_arn = "arn:aws:kms:us-east-1:123456789012:alias/layers";
        assert!(encrypt_blob(&state, "123456789012", "eu-west-1", alias_arn, b"x").is_ok());
    }
}
