//! Shared SSE-KMS body decryption.
//!
//! Objects written to an SSE-KMS bucket are stored as a fakecloud-kms envelope
//! (base64 ASCII), and only the S3 API's read paths used to unwrap it. Every
//! *internal* reader — the CloudFormation resource provisioner hydrating
//! `Code.S3Bucket`/`Code.S3Key`, the `S3Delivery` hook Lambda pulls code
//! through — read the stored bytes straight off the state and handed the
//! envelope on as if it were the object. A `cdk bootstrap` assets bucket
//! defaults to `aws:kms`, so a Lambda deployed from a CDK asset received
//! ciphertext instead of its ZIP.
//!
//! This is that unwrap in one place, so a reader cannot forget it.

use std::sync::Arc;

use fakecloud_aws::arn::Arn;
use fakecloud_core::delivery::KmsHook;

/// Unwrap `stored` when it is an SSE-KMS envelope.
///
/// `kms_key_id` is the key the object records (normally its ARN). A key ARN
/// names the account whose KMS holds the key, which may not be the bucket
/// owner's; without one the key is `account_id`'s (see
/// [`decrypt_envelope`]).
///
/// Returns the bytes unchanged when the object was not written under
/// `aws:kms`, when no KMS hook is wired, or when the bytes are not UTF-8
/// (snapshots taken before the hook existed hold plaintext). A decrypt that
/// fails with a hook present is an error rather than a silent passthrough:
/// handing a caller a raw envelope is how the ZIP corruption above happened.
pub fn decrypt_body(
    hook: Option<&Arc<dyn KmsHook>>,
    account_id: &str,
    bucket: &str,
    sse_algorithm: Option<&str>,
    kms_key_id: Option<&str>,
    stored: Vec<u8>,
) -> Result<Vec<u8>, String> {
    // Taken by value: the common case is a passthrough, and returning the
    // caller's own buffer keeps that free.
    if sse_algorithm != Some("aws:kms") {
        return Ok(stored);
    }
    let Some(hook) = hook else {
        return Ok(stored);
    };
    let Ok(envelope) = std::str::from_utf8(&stored) else {
        return Ok(stored);
    };
    decrypt_envelope(hook.as_ref(), account_id, bucket, kms_key_id, envelope)
        .map_err(|e| format!("SSE-KMS decrypt failed for s3://{bucket}: {e}"))
}

/// Decrypt one SSE-KMS envelope through the hook, returning the hook's own
/// error string unchanged so the S3 API path can map it to a typed KMS
/// error. Shared by [`decrypt_body`] and `S3Service::decrypt_object_body`.
///
/// The decrypt runs in the account the key reference names (a key or alias
/// ARN), falling back to `account_id` (the bucket owner) for a bare key id,
/// alias name, or no key at all, matching how the body was encrypted.
pub(crate) fn decrypt_envelope(
    hook: &dyn KmsHook,
    account_id: &str,
    bucket: &str,
    kms_key_id: Option<&str>,
    envelope: &str,
) -> Result<Vec<u8>, String> {
    let key_account = kms_key_id
        .filter(|k| !k.is_empty())
        .and_then(crate::service::kms_reference_account)
        .unwrap_or(account_id);
    let mut ctx = std::collections::HashMap::new();
    ctx.insert("aws:s3:arn".to_string(), Arn::s3(bucket).to_string());
    hook.decrypt(key_account, envelope, "s3.amazonaws.com", ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Reverses the trivial "encryption" used below, so a decrypt that actually
    /// ran is distinguishable from bytes passed straight through.
    struct StubKms {
        fail: bool,
        decrypted_in: std::sync::Mutex<Option<String>>,
    }

    impl KmsHook for StubKms {
        fn encrypt(
            &self,
            _account_id: &str,
            _region: &str,
            _key_id: &str,
            plaintext: &[u8],
            _service_principal: &str,
            _encryption_context: HashMap<String, String>,
        ) -> Result<String, String> {
            Ok(format!("env:{}", String::from_utf8_lossy(plaintext)))
        }

        fn decrypt(
            &self,
            account_id: &str,
            ciphertext_b64: &str,
            _service_principal: &str,
            _encryption_context: HashMap<String, String>,
        ) -> Result<Vec<u8>, String> {
            *self.decrypted_in.lock().unwrap() = Some(account_id.to_string());
            if self.fail {
                return Err("key revoked".into());
            }
            Ok(ciphertext_b64
                .strip_prefix("env:")
                .unwrap_or(ciphertext_b64)
                .as_bytes()
                .to_vec())
        }
    }

    fn stub(fail: bool) -> Arc<StubKms> {
        Arc::new(StubKms {
            fail,
            decrypted_in: std::sync::Mutex::new(None),
        })
    }

    fn hook(fail: bool) -> Arc<dyn KmsHook> {
        stub(fail)
    }

    #[test]
    fn unencrypted_object_is_returned_unchanged() {
        let h = hook(false);
        for sse in [None, Some("AES256")] {
            let out = decrypt_body(Some(&h), "1", "b", sse, None, b"env:plain".to_vec()).unwrap();
            assert_eq!(out, b"env:plain", "{sse:?}");
        }
    }

    #[test]
    fn kms_object_is_decrypted() {
        let h = hook(false);
        let out = decrypt_body(
            Some(&h),
            "1",
            "b",
            Some("aws:kms"),
            None,
            b"env:PK\x03\x04zip".to_vec(),
        )
        .unwrap();
        assert_eq!(out, b"PK\x03\x04zip");
    }

    #[test]
    fn kms_object_without_a_hook_is_returned_unchanged() {
        // No KMS wired: nothing can unwrap it, so passing it through is all
        // that's left.
        let out = decrypt_body(None, "1", "b", Some("aws:kms"), None, b"env:x".to_vec()).unwrap();
        assert_eq!(out, b"env:x");
    }

    #[test]
    fn non_utf8_body_is_treated_as_a_pre_hook_snapshot() {
        let h = hook(false);
        let raw = [0x50, 0x4b, 0x03, 0x04, 0xff, 0xfe];
        let out = decrypt_body(Some(&h), "1", "b", Some("aws:kms"), None, raw.to_vec()).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn a_failed_decrypt_is_an_error_not_a_passthrough() {
        // The whole point: handing the caller the raw envelope is what fed a
        // KMS blob to Lambda as if it were a ZIP.
        let h = hook(true);
        let err =
            decrypt_body(Some(&h), "1", "b", Some("aws:kms"), None, b"env:x".to_vec()).unwrap_err();
        assert!(err.contains("key revoked"), "{err}");
    }

    #[test]
    fn decrypts_in_the_account_the_recorded_key_arn_names() {
        // A body written under another account's key was encrypted in that
        // account's KMS, so it only decrypts there.
        let s = stub(false);
        let h: Arc<dyn KmsHook> = s.clone();
        let key = "arn:aws:kms:us-east-1:222222222222:key/abc";
        decrypt_body(
            Some(&h),
            "111111111111",
            "b",
            Some("aws:kms"),
            Some(key),
            b"env:x".to_vec(),
        )
        .unwrap();
        assert_eq!(
            s.decrypted_in.lock().unwrap().as_deref(),
            Some("222222222222")
        );
    }

    #[test]
    fn a_bare_key_id_decrypts_in_the_bucket_owners_account() {
        let s = stub(false);
        let h: Arc<dyn KmsHook> = s.clone();
        for key in [None, Some(""), Some("abc"), Some("alias/mine")] {
            decrypt_body(
                Some(&h),
                "111111111111",
                "b",
                Some("aws:kms"),
                key,
                b"env:x".to_vec(),
            )
            .unwrap();
            assert_eq!(
                s.decrypted_in.lock().unwrap().as_deref(),
                Some("111111111111"),
                "{key:?}"
            );
        }
    }
}
