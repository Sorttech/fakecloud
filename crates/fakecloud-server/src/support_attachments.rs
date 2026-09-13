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
//! shim.

use std::collections::HashMap;

use axum::body::Bytes;
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

async fn put_part(
    Path((account_id, upload_id, part_index)): Path<(String, String, i64)>,
    Query(params): Query<HashMap<String, String>>,
    State(ctx): State<SupportAttachmentRoutesContext>,
    body: Bytes,
) -> impl IntoResponse {
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
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{file_name}\""),
                ),
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
    use axum::body::Body;
    use axum::http::Request;
    use fakecloud_core::multi_account::MultiAccountState;
    use fakecloud_support::state::{AttachmentUpload, UploadPart, UPLOAD_NOT_READY};
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tower::ServiceExt;

    const ACCOUNT: &str = "000000000000";

    fn context() -> SupportAttachmentRoutesContext {
        let state: SharedSupportState = Arc::new(RwLock::new(MultiAccountState::new(
            ACCOUNT,
            "us-east-1",
            "http://localhost:4566",
        )));
        {
            let mut guard = state.write();
            let data = guard.get_or_create(ACCOUNT);
            data.attachment_uploads.insert(
                "upload-1".to_string(),
                AttachmentUpload {
                    upload_id: "upload-1".to_string(),
                    file_name: "log.txt".to_string(),
                    file_size_bytes: 5,
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
                    }],
                    attachment_id: None,
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
