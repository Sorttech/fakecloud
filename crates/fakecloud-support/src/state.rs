//! Account-partitioned, serializable state for AWS Support (`support`).
//!
//! Cases are stored as their already-output-valid `CaseDetails` wire object
//! (`serde_json::Value`, camelCase members matching the awsJson1_1 member
//! names) so a `DescribeCases` echoes exactly what `CreateCase` persisted. The
//! per-case communication thread lives in a side map keyed by case id, and
//! attachment sets / individual attachments live in their own maps so
//! `DescribeAttachment` has a single source of truth.
//!
//! The Trusted Advisor refresh status is a per-check state machine
//! (`none -> enqueued -> processing -> success`) stored in `ta_refresh`;
//! `RefreshTrustedAdvisorCheck` enqueues and each
//! `DescribeTrustedAdvisorCheckRefreshStatuses` read advances it one step.
//!
//! The presigned attachment-upload flow (`GetAttachmentUploadLinks` ->
//! `PUT` each part -> `CompleteAttachmentUpload`) keeps its own bookkeeping in
//! [`AttachmentUpload`]: one record per `uploadId` holding the issued part
//! links (each with its own signature and expiry), the bytes and `ETag`
//! actually uploaded to each link, which parts `CompleteAttachmentUpload` has
//! been called for (it may be called one part at a time), and the terminal
//! upload status. Download grants minted by `GetAttachmentDownloadLink` live
//! in `attachment_downloads` keyed by the URL signature, so the data-plane
//! route can authorise a download without re-deriving anything.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use fakecloud_core::multi_account::{AccountState, MultiAccountState};

pub const SUPPORT_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// The upload is still accepting parts; `CompleteAttachmentUpload` has not
/// succeeded yet.
pub const UPLOAD_NOT_READY: &str = "attachment-not-ready";
/// Every part was uploaded and `CompleteAttachmentUpload` assembled the
/// attachment; it is retrievable with `DescribeAttachment`.
pub const UPLOAD_READY: &str = "attachment-ready";
/// The upload can no longer be completed (its links expired).
pub const UPLOAD_FAILED: &str = "failed";

/// One part link issued by `GetAttachmentUploadLinks`, plus whatever the
/// client has since `PUT` to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadPart {
    /// 1-based part index, matching `UploadUrl.partIndex`.
    pub part_index: i64,
    /// The `X-Amz-Signature` embedded in this part's presigned URL. The
    /// data-plane route accepts a `PUT` only when it presents this value.
    pub signature: String,
    /// ISO-8601 instant after which the link stops working.
    pub expiry: String,
    /// The `ETag` returned for the uploaded bytes, `None` until the part is
    /// uploaded.
    #[serde(default)]
    pub etag: Option<String>,
    /// The uploaded bytes, base64-encoded, `None` until the part is uploaded
    /// and again once `CompleteAttachmentUpload` assembled the attachment (the
    /// bytes live in `attachments` from then on, so keeping a second copy here
    /// would double the upload's cost in memory and in every snapshot).
    #[serde(default)]
    pub data: Option<String>,
    /// Whether `CompleteAttachmentUpload` has been called for this part. The
    /// model allows completing one part per call, so completion is recorded
    /// per part and the attachment is assembled only once every part is in.
    #[serde(default)]
    pub completed: bool,
}

/// A multipart attachment upload keyed by its `uploadId`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttachmentUpload {
    pub upload_id: String,
    pub file_name: String,
    /// Size the client declared in `GetAttachmentUploadLinks`; `0` when it did
    /// not declare one (a single-part upload of unknown length).
    #[serde(default)]
    pub file_size_bytes: i64,
    pub part_size_bytes: i64,
    pub total_parts: i64,
    /// One of [`UPLOAD_NOT_READY`], [`UPLOAD_READY`], [`UPLOAD_FAILED`].
    pub status: String,
    /// ISO-8601 instant after which the upload can no longer be completed.
    pub expiry: String,
    /// Issued part links, ordered by `part_index`.
    #[serde(default)]
    pub parts: Vec<UploadPart>,
    /// Set once `CompleteAttachmentUpload` assembled the attachment.
    #[serde(default)]
    pub attachment_id: Option<String>,
}

impl AttachmentUpload {
    /// The issued link for `part_index`, if one was ever handed out.
    pub fn part(&self, part_index: i64) -> Option<&UploadPart> {
        self.parts.iter().find(|p| p.part_index == part_index)
    }

    /// Mutable access to the issued link for `part_index`.
    pub fn part_mut(&mut self, part_index: i64) -> Option<&mut UploadPart> {
        self.parts.iter_mut().find(|p| p.part_index == part_index)
    }

    /// How many parts have been uploaded so far. A part counts from the moment
    /// its bytes arrive; the completion flag keeps it counted after the
    /// assembled attachment released the payloads.
    pub fn completed_parts(&self) -> i64 {
        self.parts
            .iter()
            .filter(|p| p.data.is_some() || p.completed)
            .count() as i64
    }

    /// The lowest part index whose bytes have not arrived yet, or `None` when
    /// every part is in. Used to pick the range to hand links out for when the
    /// caller does not name one.
    pub fn next_unuploaded_index(&self) -> Option<i64> {
        (1..=self.total_parts).find(|i| {
            self.part(*i)
                .map(|p| p.data.is_none() && !p.completed)
                .unwrap_or(true)
        })
    }

    /// Whether `CompleteAttachmentUpload` has now been called for every part.
    pub fn all_parts_completed(&self) -> bool {
        self.total_parts > 0
            && (1..=self.total_parts).all(|i| self.part(i).map(|p| p.completed).unwrap_or(false))
    }

    /// The exact size, in bytes, part `part_index` must have: every part but
    /// the last is exactly `part_size_bytes` and the last carries the
    /// remainder. `None` when the upload declared no file size, in which case
    /// there is nothing to check a part against.
    pub fn expected_part_len(&self, part_index: i64) -> Option<i64> {
        if self.file_size_bytes <= 0 || self.part_size_bytes <= 0 {
            return None;
        }
        if part_index < self.total_parts {
            Some(self.part_size_bytes)
        } else {
            Some(self.file_size_bytes - self.part_size_bytes * (self.total_parts - 1))
        }
    }
}

/// A presigned download link minted by `GetAttachmentDownloadLink`, keyed in
/// state by the URL signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadGrant {
    pub attachment_id: String,
    /// ISO-8601 instant after which the link stops working.
    pub expiry: String,
}

/// Per-account AWS Support state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SupportData {
    /// The account id this partition belongs to (for id synthesis).
    #[serde(default)]
    pub account_id: String,
    /// The region this partition belongs to.
    #[serde(default)]
    pub region: String,
    /// The server's own endpoint (e.g. `http://localhost:4566`), used to build
    /// presigned attachment upload / download links that point back at this
    /// fakecloud. Empty on snapshots written before the upload flow existed;
    /// callers fall back to the request's `Host`.
    #[serde(default)]
    pub endpoint: String,

    /// Support cases keyed by `caseId`; each value is the `CaseDetails` wire
    /// object.
    #[serde(default)]
    pub cases: BTreeMap<String, Value>,
    /// Per-case communication thread keyed by `caseId`; each entry is a
    /// `Communication` wire object, newest last.
    #[serde(default)]
    pub communications: BTreeMap<String, Vec<Value>>,

    /// Attachment sets keyed by `attachmentSetId`. Each value carries
    /// `expiryTime` and the list of member `attachmentId`s.
    #[serde(default)]
    pub attachment_sets: BTreeMap<String, Value>,
    /// Individual attachments keyed by `attachmentId`; each value carries
    /// `fileName` and base64 `data` (the `Attachment` wire object).
    #[serde(default)]
    pub attachments: BTreeMap<String, Value>,

    /// Multipart attachment uploads keyed by `uploadId`.
    #[serde(default)]
    pub attachment_uploads: BTreeMap<String, AttachmentUpload>,
    /// Presigned download grants keyed by the URL's `X-Amz-Signature`.
    #[serde(default)]
    pub attachment_downloads: BTreeMap<String, DownloadGrant>,

    /// Trusted Advisor per-check refresh status keyed by `checkId`; one of
    /// `none` / `enqueued` / `processing` / `success`.
    #[serde(default)]
    pub ta_refresh: BTreeMap<String, String>,
}

impl AccountState for SupportData {
    fn new_for_account(account_id: &str, region: &str, endpoint: &str) -> Self {
        Self {
            account_id: account_id.to_string(),
            region: region.to_string(),
            endpoint: endpoint.to_string(),
            ..Default::default()
        }
    }
}

impl SupportData {
    /// The Trusted Advisor refresh state machine advances only on an explicit
    /// read, so a restart leaves it exactly as persisted. Attachment uploads
    /// do have a wall-clock lifecycle: their presigned links expire, and an
    /// upload whose links expired before it was completed can never be
    /// completed. Sweep those to `failed` on load so a restart does not
    /// resurrect an upload the client can no longer finish. Returns `true`
    /// when anything settled.
    pub fn reconcile(&mut self) -> bool {
        let now = crate::shared::iso_now();
        let mut changed = false;
        for upload in self.attachment_uploads.values_mut() {
            if upload.status == UPLOAD_NOT_READY && upload.expiry <= now {
                upload.status = UPLOAD_FAILED.to_string();
                changed = true;
            }
        }
        let grants_before = self.attachment_downloads.len();
        self.attachment_downloads
            .retain(|_, grant| grant.expiry > now);
        changed |= self.attachment_downloads.len() != grants_before;
        changed
    }

    /// Advance one check's refresh status one step toward `success`, returning
    /// the resulting status. `none`/absent stays `none` until an explicit
    /// refresh enqueues it.
    pub fn advance_refresh(&mut self, check_id: &str) -> String {
        let cur = self
            .ta_refresh
            .get(check_id)
            .cloned()
            .unwrap_or_else(|| "none".to_string());
        let next = match cur.as_str() {
            "enqueued" => "processing",
            "processing" => "success",
            other => other,
        };
        self.ta_refresh
            .insert(check_id.to_string(), next.to_string());
        next.to_string()
    }
}

pub type SharedSupportState = Arc<RwLock<MultiAccountState<SupportData>>>;

#[derive(Debug, Serialize, Deserialize)]
pub struct SupportSnapshot {
    pub schema_version: u32,
    pub accounts: MultiAccountState<SupportData>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> SupportData {
        SupportData::new_for_account("000000000000", "us-east-1", "")
    }

    #[test]
    fn refresh_advances_through_states() {
        let mut d = data();
        // Unrefreshed check stays none.
        assert_eq!(d.advance_refresh("Qch7DwouX1"), "none");
        // Enqueue, then each read advances one step.
        d.ta_refresh.insert("Qch7DwouX1".into(), "enqueued".into());
        assert_eq!(d.advance_refresh("Qch7DwouX1"), "processing");
        assert_eq!(d.advance_refresh("Qch7DwouX1"), "success");
        // Terminal: stays success.
        assert_eq!(d.advance_refresh("Qch7DwouX1"), "success");
    }

    #[test]
    fn reconcile_is_noop_without_uploads() {
        let mut d = data();
        assert!(!d.reconcile());
    }

    fn upload(expiry: &str, status: &str) -> AttachmentUpload {
        AttachmentUpload {
            upload_id: "upload-1".into(),
            file_name: "log.txt".into(),
            file_size_bytes: 4,
            part_size_bytes: 5 * 1024 * 1024,
            total_parts: 2,
            status: status.into(),
            expiry: expiry.into(),
            parts: vec![
                UploadPart {
                    part_index: 1,
                    signature: "sig1".into(),
                    expiry: expiry.into(),
                    etag: Some("\"abc\"".into()),
                    data: Some("aGk=".into()),
                    completed: false,
                },
                UploadPart {
                    part_index: 2,
                    signature: "sig2".into(),
                    expiry: expiry.into(),
                    etag: None,
                    data: None,
                    completed: false,
                },
            ],
            attachment_id: None,
        }
    }

    #[test]
    fn reconcile_fails_expired_uploads_and_prunes_grants() {
        let mut d = data();
        d.attachment_uploads.insert(
            "upload-1".into(),
            upload("2000-01-01T00:00:00.000Z", UPLOAD_NOT_READY),
        );
        d.attachment_downloads.insert(
            "sig".into(),
            DownloadGrant {
                attachment_id: "attachment-1".into(),
                expiry: "2000-01-01T00:00:00.000Z".into(),
            },
        );
        assert!(d.reconcile());
        assert_eq!(d.attachment_uploads["upload-1"].status, UPLOAD_FAILED);
        assert!(d.attachment_downloads.is_empty());
    }

    #[test]
    fn reconcile_leaves_live_uploads_alone() {
        let mut d = data();
        d.attachment_uploads.insert(
            "upload-1".into(),
            upload("2999-01-01T00:00:00.000Z", UPLOAD_NOT_READY),
        );
        assert!(!d.reconcile());
        assert_eq!(d.attachment_uploads["upload-1"].status, UPLOAD_NOT_READY);
    }

    #[test]
    fn upload_tracks_part_progress() {
        let u = upload("2999-01-01T00:00:00.000Z", UPLOAD_NOT_READY);
        assert_eq!(u.completed_parts(), 1);
        // Part 1 is in, so part 2 is the next one the client should upload.
        assert_eq!(u.next_unuploaded_index(), Some(2));
        assert_eq!(u.part(1).unwrap().signature, "sig1");
        assert!(u.part(3).is_none());
        assert!(!u.all_parts_completed());
    }

    #[test]
    fn fully_uploaded_has_no_next_index() {
        let mut u = upload("2999-01-01T00:00:00.000Z", UPLOAD_NOT_READY);
        let part = u.part_mut(2).unwrap();
        part.data = Some("eA==".into());
        part.etag = Some("\"def\"".into());
        assert_eq!(u.completed_parts(), 2);
        assert_eq!(u.next_unuploaded_index(), None);
    }

    #[test]
    fn completion_is_tracked_per_part() {
        let mut u = upload("2999-01-01T00:00:00.000Z", UPLOAD_NOT_READY);
        u.part_mut(1).unwrap().completed = true;
        // Part 2 has not been reported yet, so the upload is not assembled.
        assert!(!u.all_parts_completed());
        let part = u.part_mut(2).unwrap();
        part.data = Some("eA==".into());
        part.completed = true;
        assert!(u.all_parts_completed());
        // Dropping the payloads after assembly does not lose the progress.
        for part in &mut u.parts {
            part.data = None;
        }
        assert_eq!(u.completed_parts(), 2);
    }

    #[test]
    fn expected_part_len_splits_the_declared_size() {
        let mut u = upload("2999-01-01T00:00:00.000Z", UPLOAD_NOT_READY);
        u.part_size_bytes = 5;
        u.file_size_bytes = 8;
        // Every part but the last is exactly one part size; the last carries
        // the remainder.
        assert_eq!(u.expected_part_len(1), Some(5));
        assert_eq!(u.expected_part_len(2), Some(3));
        // An upload that declared no size has nothing to check against.
        u.file_size_bytes = 0;
        assert_eq!(u.expected_part_len(1), None);
    }
}
