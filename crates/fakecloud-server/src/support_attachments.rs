//! HTTP endpoints behind the presigned attachment links AWS Support hands out.
//!
//! `GetAttachmentUploadLinks` returns one presigned `PUT` link per part and
//! `GetAttachmentDownloadLink` returns a presigned `GET` link; real AWS points
//! both at S3, fakecloud points them at itself and serves them here so a client
//! that simply follows the URL really does transfer bytes.
//!
//! Presigned URLs are unauthenticated by design, so authorisation is the
//! `X-Amz-Signature` query parameter: `fakecloud_support::dataplane` checks it
//! against the value recorded when the link was issued, along with the link's
//! expiry. All the state handling lives there; this module is the transport
//! shim: it buffers the `PUT` body under the server-wide request cap (a part
//! can be up to 100 MB, far past axum's 2 MB extractor default) and renders a
//! download's caller-supplied file name into a header value safely.

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::Router;

use fakecloud_persistence::SnapshotHook;
use fakecloud_support::dataplane::{DownloadOutcome, PutPartOutcome};
use fakecloud_support::SharedSupportState;

/// Routes mounted under `/_fakecloud/support/attachments/*`.
#[derive(Clone)]
pub struct SupportAttachmentRoutesContext {
    pub support_state: SharedSupportState,
    /// Persist hook, `None` in memory mode. Uploaded parts are real state, so
    /// a successful `PUT` snapshots like any other Support mutation.
    pub snapshot: Option<SnapshotHook>,
}

pub fn router(ctx: SupportAttachmentRoutesContext) -> Router {
    Router::new()
        .route(
            "/_fakecloud/support/attachments/uploads/{account_id}/{upload_id}/{part_index}",
            put(put_part),
        )
        .route(
            "/_fakecloud/support/attachments/downloads/{account_id}/{attachment_id}",
            get(download_attachment),
        )
        .with_state(ctx)
}

fn signature(params: &HashMap<String, String>) -> String {
    params.get("X-Amz-Signature").cloned().unwrap_or_default()
}

/// The `Content-Disposition` header for a downloaded attachment.
///
/// The file name is whatever the caller passed to `GetAttachmentUploadLinks` /
/// `AddAttachmentsToSet`, so it is not safe to interpolate: a quote would close
/// the quoted string and let the rest of the name inject header parameters of
/// its own, and a CR/LF (or any other control character) makes a `HeaderValue`
/// that cannot be built at all, turning a download into a bare 500. Quotes and
/// backslashes are escaped the way RFC 6266's quoted-string does, anything
/// outside printable ASCII is dropped, and a name with nothing usable left
/// falls back to a generic one.
fn content_disposition(file_name: &str) -> String {
    let mut quoted = String::with_capacity(file_name.len());
    for ch in file_name.chars() {
        match ch {
            '"' | '\\' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            c if c == ' ' || c.is_ascii_graphic() => quoted.push(c),
            // Control characters (CR/LF included) and non-ASCII are dropped:
            // neither can appear verbatim in a header value.
            _ => {}
        }
    }
    if quoted.trim().is_empty() {
        quoted = "attachment".to_string();
    }
    format!("attachment; filename=\"{quoted}\"")
}

async fn put_part(
    Path((account_id, upload_id, part_index)): Path<(String, String, i64)>,
    Query(params): Query<HashMap<String, String>>,
    State(ctx): State<SupportAttachmentRoutesContext>,
    body: Body,
) -> impl IntoResponse {
    // Buffer the body by hand under the server-wide cap, exactly as the AWS
    // dispatcher does. Extracting `Bytes` instead would apply axum's default
    // 2 MB body limit, which every real part upload (up to 100 MB per the
    // model) exceeds.
    let body = match axum::body::to_bytes(body, fakecloud_core::dispatch::max_request_body_bytes())
        .await
    {
        Ok(bytes) => bytes,
        Err(_) => {
            return (StatusCode::PAYLOAD_TOO_LARGE, "part body too large").into_response();
        }
    };
    let outcome = fakecloud_support::dataplane::put_upload_part(
        &ctx.support_state,
        &account_id,
        &upload_id,
        part_index,
        &signature(&params),
        &body,
    );
    match outcome {
        PutPartOutcome::Stored(etag) => {
            if let Some(hook) = &ctx.snapshot {
                hook().await;
            }
            (StatusCode::OK, [(header::ETAG, etag)], ()).into_response()
        }
        PutPartOutcome::NotFound => {
            (StatusCode::NOT_FOUND, "no such attachment upload part").into_response()
        }
        PutPartOutcome::Forbidden => {
            (StatusCode::FORBIDDEN, "invalid presigned link signature").into_response()
        }
        PutPartOutcome::Expired => {
            (StatusCode::FORBIDDEN, "presigned link has expired").into_response()
        }
        PutPartOutcome::AlreadyCompleted => (
            StatusCode::CONFLICT,
            "the attachment upload is already complete",
        )
            .into_response(),
        PutPartOutcome::InvalidSize(expected, actual) => (
            StatusCode::BAD_REQUEST,
            format!("this part must be exactly {expected} bytes, got {actual}"),
        )
            .into_response(),
    }
}

async fn download_attachment(
    Path((account_id, attachment_id)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    State(ctx): State<SupportAttachmentRoutesContext>,
) -> impl IntoResponse {
    let outcome = fakecloud_support::dataplane::fetch_attachment(
        &ctx.support_state,
        &account_id,
        &attachment_id,
        &signature(&params),
    );
    match outcome {
        DownloadOutcome::Found(file_name, bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::CONTENT_DISPOSITION, content_disposition(&file_name)),
            ],
            bytes,
        )
            .into_response(),
        DownloadOutcome::NotFound => (StatusCode::NOT_FOUND, "no such attachment").into_response(),
        DownloadOutcome::Forbidden => {
            (StatusCode::FORBIDDEN, "invalid presigned link signature").into_response()
        }
        DownloadOutcome::Expired => {
            (StatusCode::FORBIDDEN, "presigned link has expired").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use fakecloud_core::multi_account::MultiAccountState;
    use fakecloud_support::state::{AttachmentUpload, DownloadGrant, UploadPart, UPLOAD_NOT_READY};
    use parking_lot::RwLock;
    use serde_json::json;
    use std::sync::Arc;
    use tower::ServiceExt;

    const ACCOUNT: &str = "000000000000";
    /// Comfortably past axum's 2 MB `DefaultBodyLimit`, still under one part.
    const BIG_PART_LEN: usize = 3 * 1024 * 1024;

    fn upload(file_name: &str, file_size_bytes: i64) -> AttachmentUpload {
        AttachmentUpload {
            upload_id: "unused".to_string(),
            file_name: file_name.to_string(),
            file_size_bytes,
            part_size_bytes: 5 * 1024 * 1024,
            total_parts: 1,
            status: UPLOAD_NOT_READY.to_string(),
            expiry: "2999-01-01T00:00:00.000Z".to_string(),
            parts: vec![UploadPart {
                part_index: 1,
                signature: "sig-1".to_string(),
                expiry: "2999-01-01T00:00:00.000Z".to_string(),
                etag: None,
                data: None,
                completed: false,
            }],
            attachment_id: None,
        }
    }

    fn context() -> SupportAttachmentRoutesContext {
        let state: SharedSupportState = Arc::new(RwLock::new(MultiAccountState::new(
            ACCOUNT,
            "us-east-1",
            "http://localhost:4566",
        )));
        {
            let mut guard = state.write();
            let data = guard.get_or_create(ACCOUNT);
            let mut small = upload("log.txt", 5);
            small.upload_id = "upload-1".to_string();
            data.attachment_uploads
                .insert("upload-1".to_string(), small);
            // A part big enough to prove the route is not capped at axum's
            // 2 MB extractor default.
            let mut big = upload("big.bin", BIG_PART_LEN as i64);
            big.upload_id = "upload-big".to_string();
            data.attachment_uploads
                .insert("upload-big".to_string(), big);

            // A stored attachment whose file name is hostile to a header.
            data.attachments.insert(
                "attachment-1".to_string(),
                json!({ "fileName": "re\"port\r\nX-Injected: yes.txt", "data": "aGVsbG8=" }),
            );
            data.attachment_downloads.insert(
                "dl-sig".to_string(),
                DownloadGrant {
                    attachment_id: "attachment-1".to_string(),
                    expiry: "2999-01-01T00:00:00.000Z".to_string(),
                },
            );
        }
        SupportAttachmentRoutesContext {
            support_state: state,
            snapshot: None,
        }
    }

    #[tokio::test]
    async fn put_with_a_valid_signature_stores_the_part() {
        let ctx = context();
        let state = ctx.support_state.clone();
        let response = router(ctx)
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/_fakecloud/support/attachments/uploads/000000000000/upload-1/1?X-Amz-Signature=sig-1")
                    .body(Body::from("hello"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::ETAG).unwrap(),
            "\"5d41402abc4b2a76b9719d911017c592\""
        );
        let guard = state.read();
        assert!(guard.get(ACCOUNT).unwrap().attachment_uploads["upload-1"]
            .part(1)
            .unwrap()
            .data
            .is_some());
    }

    #[tokio::test]
    async fn put_with_a_bad_signature_is_forbidden() {
        let response = router(context())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/_fakecloud/support/attachments/uploads/000000000000/upload-1/1?X-Amz-Signature=nope")
                    .body(Body::from("hello"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn put_accepts_a_part_larger_than_axums_default_body_limit() {
        let ctx = context();
        let state = ctx.support_state.clone();
        let part = vec![b'z'; BIG_PART_LEN];
        let response = router(ctx)
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/_fakecloud/support/attachments/uploads/000000000000/upload-big/1?X-Amz-Signature=sig-1")
                    .body(Body::from(part))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Extracting `Bytes` would have made this a 413 at 2 MB.
        assert_eq!(response.status(), StatusCode::OK);
        let guard = state.read();
        let stored = guard.get(ACCOUNT).unwrap().attachment_uploads["upload-big"]
            .part(1)
            .unwrap()
            .data
            .clone()
            .unwrap();
        // Base64 of n bytes is 4 * ceil(n / 3) characters.
        assert_eq!(stored.len(), 4 * BIG_PART_LEN.div_ceil(3));
    }

    #[tokio::test]
    async fn put_of_a_wrongly_sized_part_is_rejected() {
        let response = router(context())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/_fakecloud/support/attachments/uploads/000000000000/upload-1/1?X-Amz-Signature=sig-1")
                    .body(Body::from("hi"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn download_escapes_a_hostile_file_name() {
        let response = router(context())
            .oneshot(
                Request::builder()
                    .uri("/_fakecloud/support/attachments/downloads/000000000000/attachment-1?X-Amz-Signature=dl-sig")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // The CR/LF in the name would otherwise make an unbuildable header
        // value and turn the download into a bare 500.
        assert_eq!(response.status(), StatusCode::OK);
        let disposition = response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            disposition,
            "attachment; filename=\"re\\\"portX-Injected: yes.txt\""
        );
        assert!(response.headers().get("x-injected").is_none());
    }

    #[test]
    fn content_disposition_escapes_quotes_and_drops_control_characters() {
        assert_eq!(
            content_disposition("log.txt"),
            "attachment; filename=\"log.txt\""
        );
        // A quote is escaped rather than closing the quoted string, so it
        // cannot start a parameter of its own.
        assert_eq!(
            content_disposition("a\"; filename*=UTF-8''evil"),
            "attachment; filename=\"a\\\"; filename*=UTF-8''evil\""
        );
        // A backslash cannot escape the closing quote either.
        assert_eq!(content_disposition("a\\"), "attachment; filename=\"a\\\\\"");
        // CR/LF and other control characters are dropped entirely.
        assert_eq!(
            content_disposition("a\r\nX-Injected: yes\tb"),
            "attachment; filename=\"aX-Injected: yesb\""
        );
        // Nothing usable left falls back to a generic name.
        assert_eq!(
            content_disposition("\r\n\u{1}"),
            "attachment; filename=\"attachment\""
        );
        // Non-ASCII cannot appear verbatim in a header value, so it is dropped
        // rather than failing the whole download.
        assert_eq!(
            content_disposition("relat\u{f3}rio.txt"),
            "attachment; filename=\"relatrio.txt\""
        );
    }

    #[tokio::test]
    async fn download_without_a_grant_is_forbidden() {
        let response = router(context())
            .oneshot(
                Request::builder()
                    .uri("/_fakecloud/support/attachments/downloads/000000000000/attachment-1?X-Amz-Signature=nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
