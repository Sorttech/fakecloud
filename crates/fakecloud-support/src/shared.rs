//! Primitives shared across the AWS Support handlers: id synthesis, the
//! submitter identity, ISO-8601 timestamps, and the presigned attachment
//! links. Kept in one place so the create / describe paths cannot diverge on
//! wire format.

use md5::{Digest, Md5};
use serde_json::Value;

/// Current time as an ISO-8601 UTC string with millisecond precision, e.g.
/// `2013-08-23T20:10:32.000Z`. The Support `TimeCreated` / `ExpiryTime` shapes
/// carry no `@timestampFormat`, and the live service returns ISO-8601 strings.
pub fn iso_now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// The current UTC year, used in the `case-{account}-{year}-{hex}` case id.
pub fn current_year() -> i32 {
    chrono::Utc::now()
        .format("%Y")
        .to_string()
        .parse()
        .unwrap_or(2013)
}

/// FNV-1a hash for deterministic synthesis of ids from a seed so a given
/// resource's derived value is stable across reads and restarts.
pub fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A fresh 32-hex-character random id (used for the case-id tail).
pub fn hex32() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// AWS-shaped Support case id: `case-{account}-{year}-{16 hex}`.
pub fn new_case_id(account: &str, year: i32) -> String {
    let tail = &hex32()[..16];
    format!("case-{account}-{year}-{tail}")
}

/// A 10-digit numeric `displayId` derived from the case id so it round-trips.
pub fn display_id(case_id: &str) -> String {
    let n = hash_str(case_id) % 9_000_000_000 + 1_000_000_000;
    n.to_string()
}

/// A fresh attachment-set id (`as-{32 hex}`), matching the live service's
/// opaque token shape.
pub fn new_attachment_set_id() -> String {
    format!("as-{}", hex32())
}

/// A fresh attachment id (`attachment-{32 hex}`).
pub fn new_attachment_id() -> String {
    format!("attachment-{}", hex32())
}

/// A fresh attachment-upload id (`upload-{32 hex}`).
pub fn new_upload_id() -> String {
    format!("upload-{}", hex32())
}

/// A fresh 64-hex `X-Amz-Signature` for a presigned attachment link. The
/// signature is recorded in state when the link is issued and checked by the
/// data-plane route, so an unissued or tampered link is rejected the same way
/// a bad SigV4 signature would be.
pub fn new_signature() -> String {
    format!("{}{}", hex32(), hex32())
}

/// How long a presigned attachment link stays valid. AWS gives attachment
/// links an hour, the same lifetime as an attachment set.
pub const LINK_TTL_SECONDS: i64 = 3600;

/// Default part size for a multipart attachment upload: 5 MiB, S3's minimum
/// part size and the value the Support console uses.
pub const DEFAULT_PART_SIZE_BYTES: i64 = 5 * 1024 * 1024;

/// How many presigned upload URLs one `GetAttachmentUploadLinks` call returns
/// at most. The model caps both the response list ("The list contains at most
/// 10 URLs per call") and the requested range (`endIndex - startIndex` "must
/// not exceed 10") at the same number.
pub const MAX_UPLOAD_URLS_PER_CALL: i64 = 10;

/// The `X-Amz-Credential` access-key id used when the caller presented none
/// (unsigned requests are accepted by default). Matches AWS's documented
/// example key so the link keeps a realistic shape.
pub const EXAMPLE_ACCESS_KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";

/// An ISO-8601 UTC timestamp `seconds` from now, in the same format as
/// [`iso_now`].
pub fn iso_in(seconds: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(seconds))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// Path of the presigned `PUT` link for one part of an attachment upload.
pub fn upload_part_path(account_id: &str, upload_id: &str, part_index: i64) -> String {
    format!("/_fakecloud/support/attachments/uploads/{account_id}/{upload_id}/{part_index}")
}

/// Path of the presigned `GET` link for a stored attachment.
pub fn attachment_download_path(account_id: &str, attachment_id: &str) -> String {
    format!("/_fakecloud/support/attachments/downloads/{account_id}/{attachment_id}")
}

/// Build a SigV4-shaped presigned URL for `path` against this fakecloud's own
/// endpoint. The query string carries the same parameters a real presigned S3
/// attachment link does, so a client that simply follows the URL works
/// unchanged; `signature` is the opaque value recorded in state for this link.
pub fn presigned_url(
    endpoint: &str,
    path: &str,
    region: &str,
    access_key_id: &str,
    signature: &str,
    expires_in: i64,
) -> String {
    let now = chrono::Utc::now();
    let date = now.format("%Y%m%d");
    let amz_date = now.format("%Y%m%dT%H%M%SZ");
    let endpoint = endpoint.trim_end_matches('/');
    format!(
        "{endpoint}{path}?X-Amz-Algorithm=AWS4-HMAC-SHA256\
         &X-Amz-Credential={access_key_id}%2F{date}%2F{region}%2Fsupport%2Faws4_request\
         &X-Amz-Date={amz_date}&X-Amz-Expires={expires_in}\
         &X-Amz-SignedHeaders=host&X-Amz-Signature={signature}"
    )
}

/// The `ETag` a part upload reports back, matching S3's quoted 32-hex MD5.
pub fn part_etag(bytes: &[u8]) -> String {
    format!("\"{:x}\"", Md5::digest(bytes))
}

/// Read a string member from a request body.
pub fn str_member<'a>(body: &'a Value, name: &str) -> Option<&'a str> {
    body.get(name).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_aws_shapes() {
        assert!(new_upload_id().starts_with("upload-"));
        assert_eq!(new_upload_id().len(), "upload-".len() + 32);
        assert_eq!(new_signature().len(), 64);
        assert_ne!(new_signature(), new_signature());
    }

    #[test]
    fn presigned_url_carries_the_sigv4_query() {
        let url = presigned_url(
            "http://localhost:4566/",
            "/_fakecloud/support/attachments/downloads/000000000000/attachment-1",
            "us-east-1",
            EXAMPLE_ACCESS_KEY_ID,
            "abc123",
            LINK_TTL_SECONDS,
        );
        assert!(
            url.starts_with(
                "http://localhost:4566/_fakecloud/support/attachments/downloads/000000000000/attachment-1?"
            ),
            "{url}"
        );
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"), "{url}");
        assert!(
            url.contains(&format!("{EXAMPLE_ACCESS_KEY_ID}%2F")),
            "{url}"
        );
        assert!(
            url.contains("%2Fus-east-1%2Fsupport%2Faws4_request"),
            "{url}"
        );
        assert!(url.contains("X-Amz-Expires=3600"), "{url}");
        assert!(url.ends_with("X-Amz-Signature=abc123"), "{url}");
        // No doubled slash from the trailing slash on the endpoint.
        assert!(!url.contains("4566//"), "{url}");
    }

    #[test]
    fn part_etag_matches_s3() {
        // S3 reports a single-part ETag as the quoted MD5 of the bytes.
        assert_eq!(part_etag(b"hello"), "\"5d41402abc4b2a76b9719d911017c592\"");
    }

    #[test]
    fn iso_in_is_in_the_future_and_comparable() {
        let now = iso_now();
        let later = iso_in(LINK_TTL_SECONDS);
        assert!(later > now, "{later} vs {now}");
        assert!(later.ends_with('Z'));
    }
}
