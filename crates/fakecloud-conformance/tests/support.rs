//! Conformance coverage for the AWS Support presigned attachment-upload
//! operations (`GetAttachmentUploadLinks` / `CompleteAttachmentUpload` /
//! `DescribeAttachmentUploadStatus` / `GetAttachmentDownloadLink`).
//!
//! These operations are newer than any `aws-sdk-support` this workspace
//! depends on, so they are driven over raw awsJson1_1 HTTP:
//! `POST /` with `x-amz-target: AWSSupport_20130415.<Operation>`.
//!
//! The upload and download links fakecloud hands out point back at fakecloud
//! itself, so the test follows them for real: it `PUT`s the part bytes to the
//! issued link, completes the upload with the `ETag` that `PUT` returned, and
//! `GET`s the download link back, asserting the bytes survive the round trip.

mod helpers;

use fakecloud_conformance_macros::test_action;
use helpers::TestServer;
use serde_json::{json, Value};

const AUTH: &str = "AWS4-HMAC-SHA256 Credential=test/20240101/us-east-1/support/aws4_request, SignedHeaders=host, Signature=0";

/// POST an awsJson1_1 AWS Support action, returning `(status, parsed_body)`.
async fn support(server: &TestServer, op: &str, body: Value) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{}/", server.endpoint()))
        .header("content-type", "application/x-amz-json-1.1")
        .header("x-amz-target", format!("AWSSupport_20130415.{op}"))
        .header("Authorization", AUTH)
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    let parsed = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, parsed)
}

#[test_action("support", "GetAttachmentUploadLinks", checksum = "81100c3d")]
#[test_action("support", "CompleteAttachmentUpload", checksum = "b1ea9dc1")]
#[test_action("support", "DescribeAttachmentUploadStatus", checksum = "18064527")]
#[test_action("support", "GetAttachmentDownloadLink", checksum = "3768e4dd")]
#[tokio::test]
async fn support_attachment_upload_round_trip() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let contents = b"fakecloud support attachment".to_vec();

    // 1. Ask for upload links for a small single-part file.
    let (status, links) = support(
        &server,
        "GetAttachmentUploadLinks",
        json!({ "fileName": "diagnostics.log", "fileSizeBytes": contents.len() }),
    )
    .await;
    assert_eq!(status, 200, "{links}");
    let upload_id = links["uploadId"].as_str().unwrap().to_string();
    assert!(!upload_id.is_empty());
    assert_eq!(links["totalParts"], 1, "{links}");
    // Nothing has been uploaded yet, so part 1 is still outstanding.
    assert_eq!(links["nextIndex"], 1, "{links}");
    assert!(links["partSizeBytes"].as_i64().unwrap() > 0, "{links}");
    let part = &links["uploadUrls"][0];
    assert_eq!(part["partIndex"], 1, "{links}");
    assert!(part["expiryDate"].is_string(), "{links}");
    let upload_url = part["url"].as_str().unwrap().to_string();
    assert!(upload_url.contains("X-Amz-Signature="), "{upload_url}");
    assert!(
        upload_url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"),
        "{upload_url}"
    );

    // 2. The link is real: PUT the bytes and keep the ETag it returns.
    let resp = client
        .put(&upload_url)
        .body(contents.clone())
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "part upload failed: {}",
        resp.status()
    );
    let etag = resp
        .headers()
        .get("etag")
        .expect("the part upload returns an ETag")
        .to_str()
        .unwrap()
        .to_string();

    // 3. The recorded progress is visible before the upload is completed.
    let (status, progress) = support(
        &server,
        "DescribeAttachmentUploadStatus",
        json!({ "uploadId": upload_id }),
    )
    .await;
    assert_eq!(status, 200, "{progress}");
    assert_eq!(
        progress["uploadStatus"], "attachment-not-ready",
        "{progress}"
    );
    assert_eq!(progress["fileName"], "diagnostics.log", "{progress}");
    assert_eq!(progress["uploadProgress"]["totalParts"], 1, "{progress}");
    assert_eq!(
        progress["uploadProgress"]["completedPartsCount"], 1,
        "{progress}"
    );

    // 4. Complete the upload with the ETag the data plane issued.
    let (status, completed) = support(
        &server,
        "CompleteAttachmentUpload",
        json!({
            "uploadId": upload_id,
            "completedUploads": [{ "partIndex": 1, "eTag": etag }],
        }),
    )
    .await;
    assert_eq!(status, 200, "{completed}");
    assert_eq!(completed["uploadStatus"], "attachment-ready", "{completed}");

    let (_, progress) = support(
        &server,
        "DescribeAttachmentUploadStatus",
        json!({ "uploadId": upload_id }),
    )
    .await;
    assert_eq!(progress["uploadStatus"], "attachment-ready", "{progress}");

    // 5. Attach the completed upload to a case to learn its attachment id.
    let (status, created) = support(
        &server,
        "CreateCase",
        json!({
            "subject": "conformance attachment upload",
            "communicationBody": "diagnostics attached",
            "uploadIds": [upload_id],
        }),
    )
    .await;
    assert_eq!(status, 200, "{created}");
    let case_id = created["caseId"].as_str().unwrap().to_string();

    let (status, comms) = support(
        &server,
        "DescribeCommunications",
        json!({ "caseId": case_id }),
    )
    .await;
    assert_eq!(status, 200, "{comms}");
    let attachment = &comms["communications"][0]["attachments"][0];
    assert_eq!(attachment["fileName"], "diagnostics.log", "{comms}");
    let attachment_id = attachment["attachmentId"].as_str().unwrap().to_string();

    // 6. A download link for it, followed for real.
    let (status, link) = support(
        &server,
        "GetAttachmentDownloadLink",
        json!({ "attachmentId": attachment_id }),
    )
    .await;
    assert_eq!(status, 200, "{link}");
    assert_eq!(link["fileName"], "diagnostics.log", "{link}");
    assert!(link["downloadUrl"]["expiryDate"].is_string(), "{link}");
    let download_url = link["downloadUrl"]["url"].as_str().unwrap().to_string();
    assert!(download_url.contains("X-Amz-Signature="), "{download_url}");

    let resp = client.get(&download_url).send().await.unwrap();
    assert!(
        resp.status().is_success(),
        "attachment download failed: {}",
        resp.status()
    );
    assert_eq!(resp.bytes().await.unwrap().to_vec(), contents);

    // A completed upload cannot be completed again.
    let (status, err) = support(
        &server,
        "CompleteAttachmentUpload",
        json!({
            "uploadId": upload_id,
            "completedUploads": [{ "partIndex": 1, "eTag": "\"whatever\"" }],
        }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["__type"], "UploadIdNotFound", "{err}");
}

#[tokio::test]
async fn support_attachment_upload_errors() {
    let server = TestServer::start().await;

    // Unknown upload ids are UploadIdNotFound on every upload operation.
    for (op, body) in [
        (
            "GetAttachmentUploadLinks",
            json!({ "fileName": "x.log", "uploadId": "upload-missing" }),
        ),
        (
            "CompleteAttachmentUpload",
            json!({ "uploadId": "upload-missing", "completedUploads": [] }),
        ),
        (
            "DescribeAttachmentUploadStatus",
            json!({ "uploadId": "upload-missing" }),
        ),
    ] {
        let (status, err) = support(&server, op, body).await;
        assert_eq!(status, 400, "{op}: {err}");
        assert_eq!(err["__type"], "UploadIdNotFound", "{op}: {err}");
    }

    // An unknown attachment has no download link.
    let (status, err) = support(
        &server,
        "GetAttachmentDownloadLink",
        json!({ "attachmentId": "attachment-missing" }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["__type"], "AttachmentIdNotFound", "{err}");

    // An upload that was never completed cannot be attached to a case.
    let (status, links) = support(
        &server,
        "GetAttachmentUploadLinks",
        json!({ "fileName": "pending.log", "fileSizeBytes": 4 }),
    )
    .await;
    assert_eq!(status, 200, "{links}");
    let upload_id = links["uploadId"].as_str().unwrap().to_string();
    let (status, err) = support(
        &server,
        "CreateCase",
        json!({
            "subject": "premature",
            "communicationBody": "not uploaded yet",
            "uploadIds": [upload_id],
        }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["__type"], "ValidationException", "{err}");

    // Required members are still enforced ahead of any of this.
    let (status, err) = support(&server, "GetAttachmentUploadLinks", json!({})).await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["__type"], "ValidationException", "{err}");

    // A dry run is refused with the modelled exception instead of taking
    // effect.
    let (status, err) = support(
        &server,
        "GetAttachmentUploadLinks",
        json!({ "fileName": "dry.log", "fileSizeBytes": 4, "dryRun": true }),
    )
    .await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["__type"], "DryRunOperationException", "{err}");
}
