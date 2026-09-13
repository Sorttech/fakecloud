//! The attachment data plane behind the presigned links AWS Support hands out.
//!
//! `GetAttachmentUploadLinks` mints one presigned `PUT` link per part and
//! `GetAttachmentDownloadLink` mints a presigned `GET` link for a stored
//! attachment. Real AWS points those links at S3; fakecloud points them at its
//! own endpoint and serves them here, so a client that follows the URL it was
//! handed really does transfer bytes.
//!
//! Both entry points are reached by unauthenticated HTTP (that is the point of
//! a presigned URL), so authorisation is the `X-Amz-Signature` in the query
//! string: the value recorded in state when the link was issued, compared in
//! constant time, plus the link's own expiry. A link that was never issued, was
//! tampered with, or has expired is refused.
//!
//! An authorised `PUT` still has to carry the right number of bytes: the
//! declared `fileSizeBytes` and the upload's `partSizeBytes` fix every part's
//! length exactly (parts 1..n-1 are one part size, part n is the remainder), so
//! a part of any other length is refused rather than assembled into a file of
//! the wrong size.
//!
//! The server crate mounts these as routes; everything that touches Support
//! state lives here so the transport layer stays a thin shim.

use base64::Engine;

use crate::shared::{iso_now, part_etag};
use crate::state::{SharedSupportState, UPLOAD_NOT_READY};

/// Result of a presigned part upload.
#[derive(Debug, PartialEq, Eq)]
pub enum PutPartOutcome {
    /// The part was stored. Carries the `ETag` the client must echo back in
    /// `CompleteAttachmentUpload`.
    Stored(String),
    /// No such account / upload id / part index.
    NotFound,
    /// The link's signature is not the one issued for this part.
    Forbidden,
    /// The link expired, or the upload it belongs to did.
    Expired,
    /// The upload was already completed; its links no longer accept bytes.
    AlreadyCompleted,
    /// The body is not the size this part must have. Every part but the last
    /// is exactly `partSizeBytes` and the last carries the remainder of the
    /// declared `fileSizeBytes`. Carries `(expected, actual)`.
    InvalidSize(i64, i64),
}

/// Result of a presigned attachment download.
#[derive(Debug, PartialEq, Eq)]
pub enum DownloadOutcome {
    /// `(fileName, bytes)` of the stored attachment.
    Found(String, Vec<u8>),
    /// No such account, grant, or attachment.
    NotFound,
    /// The link's signature is not one that was issued.
    Forbidden,
    /// The link expired.
    Expired,
}

/// Compare two signatures without leaking their contents through timing.
fn signatures_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Store the bytes a client `PUT` to one part's presigned link.
pub fn put_upload_part(
    state: &SharedSupportState,
    account_id: &str,
    upload_id: &str,
    part_index: i64,
    signature: &str,
    body: &[u8],
) -> PutPartOutcome {
    let now = iso_now();
    let mut guard = state.write();
    let Some(data) = guard.get_mut(account_id) else {
        return PutPartOutcome::NotFound;
    };
    let Some(upload) = data.attachment_uploads.get_mut(upload_id) else {
        return PutPartOutcome::NotFound;
    };
    if upload.status != UPLOAD_NOT_READY {
        return PutPartOutcome::AlreadyCompleted;
    }
    if upload.expiry <= now {
        return PutPartOutcome::Expired;
    }
    // The part's size is fixed by the declared file size and the part size the
    // upload was split into, so a part that is not exactly that long would
    // assemble into a file of the wrong length. Checked before the bytes are
    // stored, and only after the link itself is authorised so a forged link
    // learns nothing about the upload.
    let expected_len = upload.expected_part_len(part_index);
    let Some(part) = upload.part_mut(part_index) else {
        return PutPartOutcome::NotFound;
    };
    if !signatures_match(&part.signature, signature) {
        return PutPartOutcome::Forbidden;
    }
    if part.expiry <= now {
        return PutPartOutcome::Expired;
    }
    if let Some(expected) = expected_len {
        if body.len() as i64 != expected {
            return PutPartOutcome::InvalidSize(expected, body.len() as i64);
        }
    }
    let etag = part_etag(body);
    part.data = Some(base64::engine::general_purpose::STANDARD.encode(body));
    part.etag = Some(etag.clone());
    PutPartOutcome::Stored(etag)
}

/// Serve the attachment behind a presigned download link.
pub fn fetch_attachment(
    state: &SharedSupportState,
    account_id: &str,
    attachment_id: &str,
    signature: &str,
) -> DownloadOutcome {
    let now = iso_now();
    let guard = state.read();
    let Some(data) = guard.get(account_id) else {
        return DownloadOutcome::NotFound;
    };
    let Some(grant) = data.attachment_downloads.get(signature) else {
        // An unknown signature is indistinguishable from a forged one; the
        // grant map is keyed by signature so there is nothing to compare in
        // constant time here.
        return DownloadOutcome::Forbidden;
    };
    if grant.attachment_id != attachment_id {
        return DownloadOutcome::Forbidden;
    }
    if grant.expiry <= now {
        return DownloadOutcome::Expired;
    }
    let Some(attachment) = data.attachments.get(attachment_id) else {
        return DownloadOutcome::NotFound;
    };
    let file_name = attachment
        .get("fileName")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("attachment")
        .to_string();
    let encoded = attachment
        .get("data")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap_or_default();
    DownloadOutcome::Found(file_name, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::{iso_in, LINK_TTL_SECONDS};
    use crate::state::{AttachmentUpload, DownloadGrant, SupportData, UploadPart, UPLOAD_READY};
    use fakecloud_core::multi_account::MultiAccountState;
    use parking_lot::RwLock;
    use serde_json::json;
    use std::sync::Arc;

    const ACCOUNT: &str = "000000000000";

    fn state() -> SharedSupportState {
        Arc::new(RwLock::new(MultiAccountState::new(
            ACCOUNT,
            "us-east-1",
            "http://localhost:4566",
        )))
    }

    fn seed_upload(state: &SharedSupportState, expiry: &str) {
        let mut guard = state.write();
        let data: &mut SupportData = guard.get_or_create(ACCOUNT);
        data.attachment_uploads.insert(
            "upload-1".into(),
            AttachmentUpload {
                upload_id: "upload-1".into(),
                file_name: "log.txt".into(),
                file_size_bytes: 5,
                part_size_bytes: 5 * 1024 * 1024,
                total_parts: 1,
                status: UPLOAD_NOT_READY.into(),
                expiry: expiry.into(),
                parts: vec![UploadPart {
                    part_index: 1,
                    signature: "sig-1".into(),
                    expiry: expiry.into(),
                    etag: None,
                    data: None,
                    completed: false,
                }],
                attachment_id: None,
            },
        );
    }

    /// A two-part upload of a file one byte longer than a single part.
    fn seed_multipart(state: &SharedSupportState, part_size: i64) {
        let expiry = iso_in(LINK_TTL_SECONDS);
        let mut guard = state.write();
        let data: &mut SupportData = guard.get_or_create(ACCOUNT);
        data.attachment_uploads.insert(
            "upload-2".into(),
            AttachmentUpload {
                upload_id: "upload-2".into(),
                file_name: "big.bin".into(),
                file_size_bytes: part_size + 1,
                part_size_bytes: part_size,
                total_parts: 2,
                status: UPLOAD_NOT_READY.into(),
                expiry: expiry.clone(),
                parts: (1..=2)
                    .map(|part_index| UploadPart {
                        part_index,
                        signature: format!("sig-{part_index}"),
                        expiry: expiry.clone(),
                        etag: None,
                        data: None,
                        completed: false,
                    })
                    .collect(),
                attachment_id: None,
            },
        );
    }

    #[test]
    fn put_stores_bytes_and_returns_an_etag() {
        let state = state();
        seed_upload(&state, &iso_in(LINK_TTL_SECONDS));
        let outcome = put_upload_part(&state, ACCOUNT, "upload-1", 1, "sig-1", b"hello");
        // MD5 of "hello", quoted the way S3 quotes an ETag.
        assert_eq!(
            outcome,
            PutPartOutcome::Stored("\"5d41402abc4b2a76b9719d911017c592\"".into())
        );
        let guard = state.read();
        let part = guard.get(ACCOUNT).unwrap().attachment_uploads["upload-1"]
            .part(1)
            .unwrap()
            .clone();
        assert_eq!(part.data.unwrap(), "aGVsbG8=");
    }

    #[test]
    fn put_rejects_a_forged_signature() {
        let state = state();
        seed_upload(&state, &iso_in(LINK_TTL_SECONDS));
        assert_eq!(
            put_upload_part(&state, ACCOUNT, "upload-1", 1, "sig-2", b"hello"),
            PutPartOutcome::Forbidden
        );
    }

    #[test]
    fn put_rejects_unknown_ids_and_expired_links() {
        let state = state();
        seed_upload(&state, &iso_in(LINK_TTL_SECONDS));
        assert_eq!(
            put_upload_part(&state, "999999999999", "upload-1", 1, "sig-1", b"x"),
            PutPartOutcome::NotFound
        );
        assert_eq!(
            put_upload_part(&state, ACCOUNT, "upload-missing", 1, "sig-1", b"x"),
            PutPartOutcome::NotFound
        );
        assert_eq!(
            put_upload_part(&state, ACCOUNT, "upload-1", 7, "sig-1", b"x"),
            PutPartOutcome::NotFound
        );

        seed_upload(&state, "2000-01-01T00:00:00.000Z");
        assert_eq!(
            put_upload_part(&state, ACCOUNT, "upload-1", 1, "sig-1", b"x"),
            PutPartOutcome::Expired
        );
    }

    #[test]
    fn put_rejects_a_completed_upload() {
        let state = state();
        seed_upload(&state, &iso_in(LINK_TTL_SECONDS));
        state
            .write()
            .get_mut(ACCOUNT)
            .unwrap()
            .attachment_uploads
            .get_mut("upload-1")
            .unwrap()
            .status = UPLOAD_READY.into();
        assert_eq!(
            put_upload_part(&state, ACCOUNT, "upload-1", 1, "sig-1", b"x"),
            PutPartOutcome::AlreadyCompleted
        );
    }

    #[test]
    fn put_rejects_a_part_of_the_wrong_size() {
        let state = state();
        // The seeded upload declares a 5-byte file in a single part.
        seed_upload(&state, &iso_in(LINK_TTL_SECONDS));
        assert_eq!(
            put_upload_part(&state, ACCOUNT, "upload-1", 1, "sig-1", b"hi"),
            PutPartOutcome::InvalidSize(5, 2)
        );
        // Nothing was stored, so the part is still outstanding.
        assert!(
            state.read().get(ACCOUNT).unwrap().attachment_uploads["upload-1"]
                .part(1)
                .unwrap()
                .data
                .is_none()
        );

        // A non-final part must be exactly one part size; the last part
        // carries the remainder and nothing more.
        seed_multipart(&state, 8);
        assert_eq!(
            put_upload_part(&state, ACCOUNT, "upload-2", 1, "sig-1", b"short"),
            PutPartOutcome::InvalidSize(8, 5)
        );
        assert!(matches!(
            put_upload_part(&state, ACCOUNT, "upload-2", 1, "sig-1", b"12345678"),
            PutPartOutcome::Stored(_)
        ));
        assert_eq!(
            put_upload_part(&state, ACCOUNT, "upload-2", 2, "sig-2", b"toolong"),
            PutPartOutcome::InvalidSize(1, 7)
        );
        assert!(matches!(
            put_upload_part(&state, ACCOUNT, "upload-2", 2, "sig-2", b"9"),
            PutPartOutcome::Stored(_)
        ));
    }

    #[test]
    fn put_authorises_before_it_measures() {
        let state = state();
        seed_upload(&state, &iso_in(LINK_TTL_SECONDS));
        // A forged link is refused without revealing the expected part size.
        assert_eq!(
            put_upload_part(&state, ACCOUNT, "upload-1", 1, "forged", b"hi"),
            PutPartOutcome::Forbidden
        );
    }

    fn seed_attachment(state: &SharedSupportState, expiry: &str) {
        let mut guard = state.write();
        let data: &mut SupportData = guard.get_or_create(ACCOUNT);
        data.attachments.insert(
            "attachment-1".into(),
            json!({ "fileName": "log.txt", "data": "aGVsbG8=" }),
        );
        data.attachment_downloads.insert(
            "dl-sig".into(),
            DownloadGrant {
                attachment_id: "attachment-1".into(),
                expiry: expiry.into(),
            },
        );
    }

    #[test]
    fn download_returns_the_stored_bytes() {
        let state = state();
        seed_attachment(&state, &iso_in(LINK_TTL_SECONDS));
        assert_eq!(
            fetch_attachment(&state, ACCOUNT, "attachment-1", "dl-sig"),
            DownloadOutcome::Found("log.txt".into(), b"hello".to_vec())
        );
    }

    #[test]
    fn download_rejects_forged_expired_and_unknown_links() {
        let state = state();
        seed_attachment(&state, &iso_in(LINK_TTL_SECONDS));
        assert_eq!(
            fetch_attachment(&state, ACCOUNT, "attachment-1", "nope"),
            DownloadOutcome::Forbidden
        );
        // A valid signature cannot be replayed against another attachment.
        assert_eq!(
            fetch_attachment(&state, ACCOUNT, "attachment-2", "dl-sig"),
            DownloadOutcome::Forbidden
        );
        assert_eq!(
            fetch_attachment(&state, "999999999999", "attachment-1", "dl-sig"),
            DownloadOutcome::NotFound
        );

        seed_attachment(&state, "2000-01-01T00:00:00.000Z");
        assert_eq!(
            fetch_attachment(&state, ACCOUNT, "attachment-1", "dl-sig"),
            DownloadOutcome::Expired
        );
    }

    #[test]
    fn download_of_a_deleted_attachment_is_not_found() {
        let state = state();
        seed_attachment(&state, &iso_in(LINK_TTL_SECONDS));
        state
            .write()
            .get_mut(ACCOUNT)
            .unwrap()
            .attachments
            .remove("attachment-1");
        assert_eq!(
            fetch_attachment(&state, ACCOUNT, "attachment-1", "dl-sig"),
            DownloadOutcome::NotFound
        );
    }
}
