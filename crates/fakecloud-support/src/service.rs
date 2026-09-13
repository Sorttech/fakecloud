//! AWS Support awsJson1_1 dispatch + operation handlers.
//!
//! Requests carry `X-Amz-Target: AWSSupport_20130415.<Operation>`; dispatch
//! keys off `req.action`. Every operation runs model-driven input validation
//! first (required / length / range / enum / pattern), then real,
//! account-partitioned, persisted CRUD. Dereferencing a case that does not
//! exist returns Support's canonical `CaseIdNotFound`; an unknown attachment id
//! returns `AttachmentIdNotFound`, and an unknown attachment set id returns
//! `AttachmentSetIdNotFound`.
//!
//! Attachment uploads are the presigned flow: `GetAttachmentUploadLinks`
//! records an upload and hands out presigned `PUT` links, at most ten per call,
//! for one half-open range of parts; `CompleteAttachmentUpload` verifies the
//! parts named in it against their uploaded `ETag`s and assembles the
//! attachment once every part has been reported (it may be called one part at
//! a time); `DescribeAttachmentUploadStatus` reports the
//! recorded progress, and `GetAttachmentDownloadLink` mints a presigned `GET`
//! link for a stored attachment. The links point back at this fakecloud and
//! are served by [`crate::dataplane`], so they really do transfer bytes.
//!
//! Honest gap: fakecloud runs no Trusted Advisor analysis engine and attaches
//! no live support agent. `DescribeTrustedAdvisorCheckResult` /
//! `DescribeTrustedAdvisorCheckSummaries` return well-formed all-clear result
//! shapes (zero flagged resources) rather than fabricating findings, and no
//! automated agent reply is generated. Cases, communications, attachment sets,
//! attachment uploads, severity levels, the check catalogue, and the refresh
//! state machine are all real.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use http::StatusCode;
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsService, AwsServiceError};
use fakecloud_persistence::SnapshotStore;

use crate::catalog;
use crate::shared::{
    attachment_download_path, current_year, display_id, iso_in, iso_now, new_attachment_id,
    new_attachment_set_id, new_case_id, new_signature, new_upload_id, presigned_url, str_member,
    upload_part_path, DEFAULT_PART_SIZE_BYTES, EXAMPLE_ACCESS_KEY_ID, LINK_TTL_SECONDS,
    MAX_UPLOAD_URLS_PER_CALL,
};
use crate::state::{
    AttachmentUpload, DownloadGrant, SharedSupportState, SupportData, UploadPart, UPLOAD_FAILED,
    UPLOAD_NOT_READY, UPLOAD_READY,
};

/// Every operation name in the AWS Support Smithy model (20 operations).
pub const SUPPORT_ACTIONS: &[&str] = &[
    "AddAttachmentsToSet",
    "AddCommunicationToCase",
    "CompleteAttachmentUpload",
    "CreateCase",
    "DescribeAttachment",
    "DescribeAttachmentUploadStatus",
    "DescribeCases",
    "DescribeCommunications",
    "DescribeCreateCaseOptions",
    "DescribeServices",
    "DescribeSeverityLevels",
    "DescribeSupportedLanguages",
    "DescribeTrustedAdvisorCheckRefreshStatuses",
    "DescribeTrustedAdvisorCheckResult",
    "DescribeTrustedAdvisorCheckSummaries",
    "DescribeTrustedAdvisorChecks",
    "GetAttachmentDownloadLink",
    "GetAttachmentUploadLinks",
    "RefreshTrustedAdvisorCheck",
    "ResolveCase",
];

/// Operations whose Smithy shape declares `DryRunOperationException`. A
/// `dryRun: true` request against one of these is validated and then rejected
/// with that error instead of taking effect, which is what AWS does; the
/// Trusted Advisor operations do not model it and ignore the member.
const DRY_RUN_ACTIONS: &[&str] = &[
    "AddAttachmentsToSet",
    "AddCommunicationToCase",
    "CompleteAttachmentUpload",
    "CreateCase",
    "DescribeAttachment",
    "DescribeAttachmentUploadStatus",
    "DescribeCases",
    "DescribeCommunications",
    "DescribeCreateCaseOptions",
    "DescribeServices",
    "DescribeSeverityLevels",
    "DescribeSupportedLanguages",
    "GetAttachmentDownloadLink",
    "GetAttachmentUploadLinks",
    "ResolveCase",
];

/// Read-only verbs; any other action mutates persisted state and triggers a
/// snapshot after success. The inverse formulation guarantees no mutation is
/// ever missed if a new op is added: the attachment-upload operations
/// (`GetAttachmentUploadLinks`, `CompleteAttachmentUpload`,
/// `GetAttachmentDownloadLink`) all record issued links or assembled
/// attachments and are correctly classified as mutating by it.
/// `DescribeTrustedAdvisorCheckRefreshStatuses` advances the refresh state
/// machine in memory but is intentionally treated as read-only (the transition
/// is re-derived on the next read, so persisting it is not required).
fn is_mutating_action(action: &str) -> bool {
    !action.starts_with("Describe")
}

pub struct SupportService {
    state: SharedSupportState,
    snapshot_store: Option<Arc<dyn SnapshotStore>>,
    snapshot_lock: Arc<AsyncMutex<()>>,
}

impl SupportService {
    pub fn new(state: SharedSupportState) -> Self {
        Self {
            state,
            snapshot_store: None,
            snapshot_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    pub fn with_snapshot_store(mut self, store: Arc<dyn SnapshotStore>) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    async fn save(&self) {
        crate::persistence::save_snapshot(
            &self.state,
            self.snapshot_store.clone(),
            &self.snapshot_lock,
        )
        .await;
    }

    /// Persist hook for the CloudFormation provisioner; `None` in memory mode.
    pub fn snapshot_hook(&self) -> Option<fakecloud_persistence::SnapshotHook> {
        let store = self.snapshot_store.clone()?;
        let state = self.state.clone();
        let lock = self.snapshot_lock.clone();
        Some(Arc::new(move || {
            let state = state.clone();
            let store = store.clone();
            let lock = lock.clone();
            Box::pin(async move {
                crate::persistence::save_snapshot(&state, Some(store), &lock).await;
            })
        }))
    }

    /// Run `f` against this account's mutable state.
    fn with_account_mut<R>(&self, req: &AwsRequest, f: impl FnOnce(&mut SupportData) -> R) -> R {
        let mut guard = self.state.write();
        let acct = guard.get_or_create(&req.account_id);
        f(acct)
    }
}

#[async_trait]
impl AwsService for SupportService {
    fn service_name(&self) -> &str {
        "support"
    }

    async fn handle(&self, req: AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let action = req.action.clone();
        let body = req.json_body();
        if let Err(msg) = crate::validate::validate_input(&action, &body) {
            return Err(validation_error(msg));
        }
        // A dry run is validated like any other request and then refused
        // without touching state, so nothing is persisted for it.
        if is_dry_run(&body) && DRY_RUN_ACTIONS.contains(&action.as_str()) {
            return Err(dry_run_error(&action));
        }
        let result = self.dispatch(&action, &req);
        if is_mutating_action(&action)
            && matches!(result.as_ref(), Ok(resp) if resp.status.is_success())
        {
            self.save().await;
        }
        result
    }

    fn supported_actions(&self) -> &[&str] {
        SUPPORT_ACTIONS
    }
}

impl SupportService {
    fn dispatch(&self, action: &str, req: &AwsRequest) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        match action {
            // Support Cases
            "CreateCase" => self.create_case(req, &body),
            "DescribeCases" => self.describe_cases(req, &body),
            "DescribeCommunications" => self.describe_communications(req, &body),
            "AddCommunicationToCase" => self.add_communication_to_case(req, &body),
            "ResolveCase" => self.resolve_case(req, &body),
            "AddAttachmentsToSet" => self.add_attachments_to_set(req, &body),
            "DescribeAttachment" => self.describe_attachment(req, &body),
            // Presigned attachment uploads / downloads
            "GetAttachmentUploadLinks" => self.get_attachment_upload_links(req, &body),
            "CompleteAttachmentUpload" => self.complete_attachment_upload(req, &body),
            "DescribeAttachmentUploadStatus" => self.describe_attachment_upload_status(req, &body),
            "GetAttachmentDownloadLink" => self.get_attachment_download_link(req, &body),
            // Trusted Advisor
            "DescribeTrustedAdvisorChecks" => self.describe_ta_checks(&body),
            "DescribeTrustedAdvisorCheckResult" => self.describe_ta_check_result(&body),
            "DescribeTrustedAdvisorCheckSummaries" => self.describe_ta_check_summaries(&body),
            "DescribeTrustedAdvisorCheckRefreshStatuses" => {
                self.describe_ta_refresh_statuses(req, &body)
            }
            "RefreshTrustedAdvisorCheck" => self.refresh_ta_check(req, &body),
            // Severity levels + case-creation reference data
            "DescribeSeverityLevels" => self.describe_severity_levels(&body),
            "DescribeServices" => self.describe_services(&body),
            "DescribeCreateCaseOptions" => self.describe_create_case_options(&body),
            "DescribeSupportedLanguages" => self.describe_supported_languages(&body),
            _ => Err(AwsServiceError::action_not_implemented(
                self.service_name(),
                action,
            )),
        }
    }

    // ---- Support Cases ----------------------------------------------------

    fn create_case(&self, req: &AwsRequest, body: &Value) -> Result<AwsResponse, AwsServiceError> {
        let account = req.account_id.clone();
        let subject = str_member(body, "subject").unwrap_or_default().to_string();
        let comm_body = str_member(body, "communicationBody")
            .unwrap_or_default()
            .to_string();
        let service_code = str_member(body, "serviceCode")
            .unwrap_or("general-info-and-getting-started")
            .to_string();
        let severity_code = str_member(body, "severityCode")
            .unwrap_or("normal")
            .to_string();
        let category_code = str_member(body, "categoryCode")
            .unwrap_or("other")
            .to_string();
        let language = str_member(body, "language").unwrap_or("en").to_string();
        let cc = body
            .get("ccEmailAddresses")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let attachment_set_id = str_member(body, "attachmentSetId").map(str::to_string);
        let upload_ids = string_list(body, "uploadIds");
        let submitted_by = format!("arn:aws:iam::{account}:root");
        let now = iso_now();

        self.with_account_mut(req, |d| {
            // Resolve the attachment set for the initial communication, if any.
            let attachment_set = match &attachment_set_id {
                Some(id) => {
                    let set = d
                        .attachment_sets
                        .get(id)
                        .ok_or_else(|| attachment_set_id_not_found(id))?;
                    attachment_set_summary(d, set)
                }
                None => json!([]),
            };
            let attachments = communication_attachments(
                &attachment_set,
                uploads_to_attachment_details(d, &upload_ids)?,
            );

            let case_id = new_case_id(&account, current_year());
            let disp = display_id(&case_id);
            let case = json!({
                "caseId": case_id,
                "displayId": disp,
                "subject": subject,
                "status": "opened",
                "serviceCode": service_code,
                "categoryCode": category_code,
                "severityCode": severity_code,
                "submittedBy": submitted_by,
                "timeCreated": now,
                "ccEmailAddresses": cc,
                "language": language,
            });
            let comm = json!({
                "caseId": case_id,
                "body": comm_body,
                "submittedBy": submitted_by,
                "timeCreated": now,
                "attachmentSet": attachment_set,
                "attachments": attachments,
            });
            d.cases.insert(case_id.clone(), case);
            d.communications.insert(case_id.clone(), vec![comm]);
            Ok(ok(json!({ "caseId": case_id })))
        })
    }

    fn describe_cases(
        &self,
        req: &AwsRequest,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let case_id_list: Option<Vec<String>> =
            body.get("caseIdList").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            });
        let display_id_filter = str_member(body, "displayId").map(str::to_string);
        let after = str_member(body, "afterTime").map(str::to_string);
        let before = str_member(body, "beforeTime").map(str::to_string);
        let include_resolved = body
            .get("includeResolvedCases")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let include_comms = body
            .get("includeCommunications")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let max = body
            .get("maxResults")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let start = decode_token(str_member(body, "nextToken"));

        self.with_account_mut(req, |d| {
            // Collect the candidate case ids in a deterministic order.
            let mut selected: Vec<Value> = Vec::new();
            if let Some(ids) = &case_id_list {
                for id in ids {
                    let case = d.cases.get(id).ok_or_else(|| case_id_not_found(id))?;
                    selected.push(case.clone());
                }
            } else {
                for case in d.cases.values() {
                    selected.push(case.clone());
                }
            }

            // Apply the non-id filters.
            selected.retain(|c| {
                let status = c.get("status").and_then(Value::as_str).unwrap_or("");
                if !include_resolved && status == "resolved" {
                    return false;
                }
                if let Some(disp) = &display_id_filter {
                    if c.get("displayId").and_then(Value::as_str) != Some(disp.as_str()) {
                        return false;
                    }
                }
                let created = c.get("timeCreated").and_then(Value::as_str).unwrap_or("");
                if let Some(a) = &after {
                    if created < a.as_str() {
                        return false;
                    }
                }
                if let Some(b) = &before {
                    if created > b.as_str() {
                        return false;
                    }
                }
                true
            });

            // Paginate.
            let (page, next) = paginate(&selected, start, max);
            let cases: Vec<Value> = page
                .iter()
                .map(|c| {
                    let mut c = c.clone();
                    let cid = c
                        .get("caseId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if include_comms {
                        let comms = d.communications.get(&cid).cloned().unwrap_or_default();
                        c.as_object_mut().unwrap().insert(
                            "recentCommunications".to_string(),
                            json!({ "communications": comms }),
                        );
                    }
                    c
                })
                .collect();

            let mut out = json!({ "cases": cases });
            if let Some(tok) = next {
                out.as_object_mut()
                    .unwrap()
                    .insert("nextToken".to_string(), json!(tok));
            }
            Ok(ok(out))
        })
    }

    fn describe_communications(
        &self,
        req: &AwsRequest,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let case_id = str_member(body, "caseId").unwrap_or_default().to_string();
        let after = str_member(body, "afterTime").map(str::to_string);
        let before = str_member(body, "beforeTime").map(str::to_string);
        let max = body
            .get("maxResults")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let start = decode_token(str_member(body, "nextToken"));

        self.with_account_mut(req, |d| {
            if !d.cases.contains_key(&case_id) {
                return Err(case_id_not_found(&case_id));
            }
            let all = d.communications.get(&case_id).cloned().unwrap_or_default();
            let filtered: Vec<Value> = all
                .into_iter()
                .filter(|c| {
                    let created = c.get("timeCreated").and_then(Value::as_str).unwrap_or("");
                    after.as_deref().map(|a| created >= a).unwrap_or(true)
                        && before.as_deref().map(|b| created <= b).unwrap_or(true)
                })
                .collect();
            let (page, next) = paginate(&filtered, start, max);
            let mut out = json!({ "communications": page });
            if let Some(tok) = next {
                out.as_object_mut()
                    .unwrap()
                    .insert("nextToken".to_string(), json!(tok));
            }
            Ok(ok(out))
        })
    }

    fn add_communication_to_case(
        &self,
        req: &AwsRequest,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let account = req.account_id.clone();
        let case_id = str_member(body, "caseId").unwrap_or_default().to_string();
        let comm_body = str_member(body, "communicationBody")
            .unwrap_or_default()
            .to_string();
        let attachment_set_id = str_member(body, "attachmentSetId").map(str::to_string);
        let upload_ids = string_list(body, "uploadIds");
        let submitted_by = format!("arn:aws:iam::{account}:root");
        let now = iso_now();

        self.with_account_mut(req, |d| {
            if !d.cases.contains_key(&case_id) {
                return Err(case_id_not_found(&case_id));
            }
            let attachment_set = match &attachment_set_id {
                Some(id) => {
                    let set = d
                        .attachment_sets
                        .get(id)
                        .ok_or_else(|| attachment_set_id_not_found(id))?;
                    attachment_set_summary(d, set)
                }
                None => json!([]),
            };
            let attachments = communication_attachments(
                &attachment_set,
                uploads_to_attachment_details(d, &upload_ids)?,
            );
            let comm = json!({
                "caseId": case_id,
                "body": comm_body,
                "submittedBy": submitted_by,
                "timeCreated": now,
                "attachmentSet": attachment_set,
                "attachments": attachments,
            });
            d.communications
                .entry(case_id.clone())
                .or_default()
                .push(comm);
            Ok(ok(json!({ "result": true })))
        })
    }

    fn resolve_case(&self, req: &AwsRequest, body: &Value) -> Result<AwsResponse, AwsServiceError> {
        let case_id = str_member(body, "caseId").unwrap_or_default().to_string();
        self.with_account_mut(req, |d| {
            let case = d
                .cases
                .get_mut(&case_id)
                .ok_or_else(|| case_id_not_found(&case_id))?;
            let initial = case
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("opened")
                .to_string();
            case.as_object_mut()
                .unwrap()
                .insert("status".to_string(), json!("resolved"));
            Ok(ok(json!({
                "initialCaseStatus": initial,
                "finalCaseStatus": "resolved",
            })))
        })
    }

    fn add_attachments_to_set(
        &self,
        req: &AwsRequest,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let requested_set = str_member(body, "attachmentSetId").map(str::to_string);
        let attachments = body
            .get("attachments")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // Attachment sets expire one hour after their last modification.
        let expiry = (chrono::Utc::now() + chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();

        self.with_account_mut(req, |d| {
            let set_id = match &requested_set {
                Some(id) => {
                    if !d.attachment_sets.contains_key(id) {
                        return Err(attachment_set_id_not_found(id));
                    }
                    id.clone()
                }
                None => new_attachment_set_id(),
            };

            let mut ids: Vec<String> = d
                .attachment_sets
                .get(&set_id)
                .and_then(|s| s.get("attachmentIds"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            for att in &attachments {
                let att_id = new_attachment_id();
                let file_name = att.get("fileName").cloned().unwrap_or(json!(""));
                let data = att.get("data").cloned().unwrap_or(json!(""));
                d.attachments.insert(
                    att_id.clone(),
                    json!({ "fileName": file_name, "data": data }),
                );
                ids.push(att_id);
            }

            d.attachment_sets.insert(
                set_id.clone(),
                json!({ "attachmentIds": ids, "expiryTime": expiry }),
            );
            Ok(ok(json!({
                "attachmentSetId": set_id,
                "expiryTime": expiry,
            })))
        })
    }

    fn describe_attachment(
        &self,
        req: &AwsRequest,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let attachment_id = str_member(body, "attachmentId")
            .unwrap_or_default()
            .to_string();
        self.with_account_mut(req, |d| {
            let att = d
                .attachments
                .get(&attachment_id)
                .ok_or_else(|| attachment_id_not_found(&attachment_id))?;
            Ok(ok(json!({ "attachment": att })))
        })
    }

    // ---- Presigned attachment uploads / downloads -------------------------

    /// Start (or resume) a multipart attachment upload and hand out presigned
    /// `PUT` links for one range of parts. Every link is recorded in state with
    /// its own signature and expiry, so the data-plane route can authorise the
    /// `PUT` that follows.
    ///
    /// `uploadRange` is half-open: `startIndex` is inclusive, `endIndex` is
    /// exclusive, and the range may cover at most [`MAX_UPLOAD_URLS_PER_CALL`]
    /// parts, which is also the cap on a call that names no range. A caller
    /// pages through a large file by feeding the returned `nextIndex` back as
    /// the next `startIndex`.
    fn get_attachment_upload_links(
        &self,
        req: &AwsRequest,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let account = req.account_id.clone();
        let file_name = str_member(body, "fileName").unwrap_or_default().to_string();
        let file_size = body
            .get("fileSizeBytes")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let requested_upload = str_member(body, "uploadId").map(str::to_string);
        let range_start = body
            .pointer("/uploadRange/startIndex")
            .and_then(Value::as_i64);
        let range_end = body
            .pointer("/uploadRange/endIndex")
            .and_then(Value::as_i64);
        let access_key = req
            .access_key_id
            .clone()
            .unwrap_or_else(|| EXAMPLE_ACCESS_KEY_ID.to_string());
        let now = iso_now();
        let expiry = iso_in(LINK_TTL_SECONDS);

        self.with_account_mut(req, |d| {
            let endpoint = link_endpoint(req, d);
            let region = link_region(req, d);

            // A new upload is built but not stored until the requested range
            // has been validated, so a rejected call leaves no orphan behind.
            let (upload_id, new_upload) = match &requested_upload {
                Some(id) => {
                    let existing = d
                        .attachment_uploads
                        .get(id)
                        .ok_or_else(|| upload_id_not_found(id))?;
                    if existing.status == UPLOAD_READY {
                        return Err(upload_already_completed(id));
                    }
                    if existing.status == UPLOAD_FAILED || existing.expiry <= now {
                        return Err(upload_expired(id));
                    }
                    (id.clone(), None)
                }
                None => {
                    let id = new_upload_id();
                    // An undeclared size is a single-part upload; a declared
                    // one is split into 5 MiB parts like the console does.
                    let total_parts = if file_size <= 0 {
                        1
                    } else {
                        (file_size as u64).div_ceil(DEFAULT_PART_SIZE_BYTES as u64) as i64
                    };
                    (
                        id.clone(),
                        Some(AttachmentUpload {
                            upload_id: id,
                            file_name: file_name.clone(),
                            file_size_bytes: file_size.max(0),
                            part_size_bytes: DEFAULT_PART_SIZE_BYTES,
                            total_parts,
                            status: UPLOAD_NOT_READY.to_string(),
                            expiry: expiry.clone(),
                            parts: Vec::new(),
                            attachment_id: None,
                        }),
                    )
                }
            };

            // Without an explicit start, resume at the first part whose bytes
            // have not arrived; `endIndex` is exclusive and defaults to a full
            // page of links, capped by the part count.
            let (total_parts, default_start) = match &new_upload {
                Some(upload) => (upload.total_parts, 1),
                None => {
                    let upload = &d.attachment_uploads[&upload_id];
                    (
                        upload.total_parts,
                        upload.next_unuploaded_index().unwrap_or(1),
                    )
                }
            };
            let start = range_start.unwrap_or(default_start);
            let end = range_end.unwrap_or(start + MAX_UPLOAD_URLS_PER_CALL);
            let end = validate_upload_range(&upload_id, start, end, total_parts)?;
            if let Some(upload) = new_upload {
                d.attachment_uploads.insert(upload_id.clone(), upload);
            }

            let upload = d
                .attachment_uploads
                .get_mut(&upload_id)
                .expect("upload was just inserted or looked up");
            // A part link never outlives the upload it belongs to, so an
            // upload's deadline cannot be pushed back by asking for links
            // again.
            let link_expiry = expiry.min(upload.expiry.clone());

            let mut upload_urls = Vec::new();
            for part_index in start..end {
                // A part that already has a live link keeps it: the caller may
                // still be uploading to the URL it was handed, and rotating the
                // signature would break it mid-flight. Only a part with no link
                // yet, or one whose link has expired, gets a fresh signature.
                let reusable = upload
                    .part(part_index)
                    .filter(|p| p.expiry > now)
                    .map(|p| (p.signature.clone(), p.expiry.clone()));
                let (signature, part_expiry) = match reusable {
                    Some(existing) => existing,
                    None => {
                        let signature = new_signature();
                        match upload.part_mut(part_index) {
                            Some(part) => {
                                part.signature = signature.clone();
                                part.expiry = link_expiry.clone();
                            }
                            None => upload.parts.push(UploadPart {
                                part_index,
                                signature: signature.clone(),
                                expiry: link_expiry.clone(),
                                etag: None,
                                data: None,
                                completed: false,
                            }),
                        }
                        (signature, link_expiry.clone())
                    }
                };
                let url = presigned_url(
                    &endpoint,
                    &upload_part_path(&account, &upload_id, part_index),
                    &region,
                    &access_key,
                    &signature,
                    LINK_TTL_SECONDS,
                );
                upload_urls.push(json!({
                    "url": url,
                    "partIndex": part_index,
                    "expiryDate": part_expiry,
                }));
            }
            upload.parts.sort_by_key(|p| p.part_index);

            // `nextIndex` is where the caller should ask for the next page of
            // links: the part after the last one this call covered, or null
            // once links for every part have been returned.
            let next_index = if end > total_parts {
                Value::Null
            } else {
                json!(end)
            };
            Ok(ok(json!({
                "uploadId": upload_id,
                "partSizeBytes": upload.part_size_bytes,
                "totalParts": total_parts,
                "nextIndex": next_index,
                "uploadUrls": upload_urls,
            })))
        })
    }

    /// Report one or more parts of an upload as complete. Each named part must
    /// have been `PUT` to its link and the client must echo back the `ETag`
    /// that `PUT` returned; the model allows one part per call or several, so
    /// only the parts named here are validated and recorded. Once every part
    /// has been reported the parts are concatenated into a real attachment,
    /// retrievable with `DescribeAttachment` / `GetAttachmentDownloadLink` and
    /// attachable to a case through `uploadIds`; until then the upload stays
    /// `attachment-not-ready`.
    fn complete_attachment_upload(
        &self,
        req: &AwsRequest,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let upload_id = str_member(body, "uploadId").unwrap_or_default().to_string();
        let claimed: Vec<(i64, String)> = body
            .get("completedUploads")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|c| {
                        (
                            c.get("partIndex").and_then(Value::as_i64).unwrap_or(0),
                            c.get("eTag")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let now = iso_now();

        self.with_account_mut(req, |d| {
            let upload = d
                .attachment_uploads
                .get_mut(&upload_id)
                .ok_or_else(|| upload_id_not_found(&upload_id))?;
            if upload.status == UPLOAD_READY {
                return Err(upload_already_completed(&upload_id));
            }
            if upload.status == UPLOAD_FAILED || upload.expiry <= now {
                return Err(upload_expired(&upload_id));
            }
            // A call that names no part would report nothing as complete;
            // `completedUploads` is required precisely so it says which parts
            // this call finishes.
            if claimed.is_empty() {
                return Err(validation_error(format!(
                    "completedUploads must name at least one part of upload {upload_id}."
                )));
            }

            // Validate every part named in this call before recording any of
            // them, so a call that names a bad part changes nothing.
            for (part_index, claimed_etag) in &claimed {
                let part = upload.part(*part_index).ok_or_else(|| {
                    validation_error(format!("Upload {upload_id} has no part {part_index}."))
                })?;
                let (Some(stored_etag), Some(_)) = (&part.etag, &part.data) else {
                    return Err(validation_error(format!(
                        "Part {part_index} of upload {upload_id} was never uploaded."
                    )));
                };
                if !etags_match(stored_etag, claimed_etag) {
                    return Err(validation_error(format!(
                        "The eTag given for part {part_index} of upload {upload_id} does not match the uploaded part."
                    )));
                }
            }
            for (part_index, _) in &claimed {
                if let Some(part) = upload.part_mut(*part_index) {
                    part.completed = true;
                }
            }

            // The service assembles the file only after every part has been
            // reported; until then the upload keeps accepting further calls.
            if !upload.all_parts_completed() {
                return Ok(ok(json!({ "uploadStatus": UPLOAD_NOT_READY })));
            }

            // Take the parts out rather than cloning them: the payloads are
            // the whole file, and once it is assembled they are dropped so the
            // bytes are not kept twice in memory and in every snapshot.
            let mut parts = std::mem::take(&mut upload.parts);
            let file_name = upload.file_name.clone();
            let file_size_bytes = upload.file_size_bytes;
            let total_parts = upload.total_parts;

            let bytes = match assemble_parts(&upload_id, &parts, total_parts, file_size_bytes) {
                Ok(bytes) => bytes,
                Err(err) => {
                    // Put the parts back so the uploaded bytes are not lost and
                    // the caller can retry.
                    d.attachment_uploads
                        .get_mut(&upload_id)
                        .expect("looked up above")
                        .parts = parts;
                    return Err(err);
                }
            };
            for part in &mut parts {
                part.data = None;
            }

            let attachment_id = new_attachment_id();
            d.attachments.insert(
                attachment_id.clone(),
                json!({
                    "fileName": file_name,
                    "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                }),
            );
            let stored = d
                .attachment_uploads
                .get_mut(&upload_id)
                .expect("looked up above");
            stored.parts = parts;
            stored.status = UPLOAD_READY.to_string();
            stored.attachment_id = Some(attachment_id);

            Ok(ok(json!({ "uploadStatus": UPLOAD_READY })))
        })
    }

    fn describe_attachment_upload_status(
        &self,
        req: &AwsRequest,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let upload_id = str_member(body, "uploadId").unwrap_or_default().to_string();
        let now = iso_now();
        self.with_account_mut(req, |d| {
            let upload = d
                .attachment_uploads
                .get(&upload_id)
                .ok_or_else(|| upload_id_not_found(&upload_id))?;
            // An upload whose links expired before it was completed can never
            // be completed, so report it as failed even when the sweep that
            // records that has not run since it expired.
            let status = if upload.status == UPLOAD_NOT_READY && upload.expiry <= now {
                UPLOAD_FAILED
            } else {
                upload.status.as_str()
            };
            Ok(ok(json!({
                "uploadStatus": status,
                "fileName": upload.file_name,
                "uploadProgress": {
                    "totalParts": upload.total_parts,
                    "completedPartsCount": upload.completed_parts(),
                },
            })))
        })
    }

    fn get_attachment_download_link(
        &self,
        req: &AwsRequest,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let account = req.account_id.clone();
        let attachment_id = str_member(body, "attachmentId")
            .unwrap_or_default()
            .to_string();
        let access_key = req
            .access_key_id
            .clone()
            .unwrap_or_else(|| EXAMPLE_ACCESS_KEY_ID.to_string());
        let expiry = iso_in(LINK_TTL_SECONDS);

        self.with_account_mut(req, |d| {
            let file_name = d
                .attachments
                .get(&attachment_id)
                .ok_or_else(|| attachment_id_not_found(&attachment_id))?
                .get("fileName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let endpoint = link_endpoint(req, d);
            let region = link_region(req, d);
            let signature = new_signature();
            let url = presigned_url(
                &endpoint,
                &attachment_download_path(&account, &attachment_id),
                &region,
                &access_key,
                &signature,
                LINK_TTL_SECONDS,
            );
            // Each link mints a grant keyed by its own signature, so a
            // long-running process would otherwise accumulate dead grants
            // until the next snapshot load swept them. Settle the expired ones
            // (grants and uploads alike) before adding another.
            d.reconcile();
            d.attachment_downloads.insert(
                signature,
                DownloadGrant {
                    attachment_id: attachment_id.clone(),
                    expiry: expiry.clone(),
                },
            );
            Ok(ok(json!({
                "fileName": file_name,
                "downloadUrl": { "url": url, "expiryDate": expiry },
            })))
        })
    }

    // ---- Trusted Advisor --------------------------------------------------

    fn describe_ta_checks(&self, _body: &Value) -> Result<AwsResponse, AwsServiceError> {
        Ok(ok(json!({ "checks": catalog::ta_checks() })))
    }

    fn describe_ta_check_result(&self, body: &Value) -> Result<AwsResponse, AwsServiceError> {
        let check_id = str_member(body, "checkId").unwrap_or_default().to_string();
        Ok(ok(json!({
            "result": ta_check_result(&check_id),
        })))
    }

    fn describe_ta_check_summaries(&self, body: &Value) -> Result<AwsResponse, AwsServiceError> {
        let ids = string_list(body, "checkIds");
        let summaries: Vec<Value> = ids.iter().map(|id| ta_check_summary(id)).collect();
        Ok(ok(json!({ "summaries": summaries })))
    }

    fn describe_ta_refresh_statuses(
        &self,
        req: &AwsRequest,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let ids = string_list(body, "checkIds");
        let statuses = self.with_account_mut(req, |d| {
            ids.iter()
                .map(|id| {
                    let status = d.advance_refresh(id);
                    json!({
                        "checkId": id,
                        "status": status,
                        "millisUntilNextRefreshable": refresh_interval_ms(&status),
                    })
                })
                .collect::<Vec<Value>>()
        });
        Ok(ok(json!({ "statuses": statuses })))
    }

    fn refresh_ta_check(
        &self,
        req: &AwsRequest,
        body: &Value,
    ) -> Result<AwsResponse, AwsServiceError> {
        let check_id = str_member(body, "checkId").unwrap_or_default().to_string();
        self.with_account_mut(req, |d| {
            d.ta_refresh
                .insert(check_id.clone(), "enqueued".to_string());
        });
        Ok(ok(json!({
            "status": {
                "checkId": check_id,
                "status": "enqueued",
                "millisUntilNextRefreshable": refresh_interval_ms("enqueued"),
            }
        })))
    }

    // ---- Severity levels + reference data ---------------------------------

    fn describe_severity_levels(&self, _body: &Value) -> Result<AwsResponse, AwsServiceError> {
        Ok(ok(json!({ "severityLevels": catalog::severity_levels() })))
    }

    fn describe_services(&self, body: &Value) -> Result<AwsResponse, AwsServiceError> {
        let filter = body
            .get("serviceCodeList")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            });
        Ok(ok(json!({
            "services": catalog::services(filter.as_deref()),
        })))
    }

    fn describe_create_case_options(&self, _body: &Value) -> Result<AwsResponse, AwsServiceError> {
        Ok(ok(json!({
            "languageAvailability": "AVAILABLE",
            "communicationTypes": [
                { "type": "web", "supportedHours": [], "datesWithoutSupport": [] }
            ],
        })))
    }

    fn describe_supported_languages(&self, _body: &Value) -> Result<AwsResponse, AwsServiceError> {
        Ok(ok(json!({
            "supportedLanguages": [
                { "code": "en", "language": "English", "display": "English" },
                { "code": "ja", "language": "Japanese", "display": "日本語" }
            ],
        })))
    }
}

// ---- Trusted Advisor result builders -------------------------------------

fn ta_check_result(check_id: &str) -> Value {
    json!({
        "checkId": check_id,
        "timestamp": iso_now(),
        "status": "ok",
        "resourcesSummary": zero_resources_summary(),
        "categorySpecificSummary": category_specific_summary(check_id),
        "flaggedResources": [],
    })
}

fn ta_check_summary(check_id: &str) -> Value {
    json!({
        "checkId": check_id,
        "timestamp": iso_now(),
        "status": "ok",
        "hasFlaggedResources": false,
        "resourcesSummary": zero_resources_summary(),
        "categorySpecificSummary": category_specific_summary(check_id),
    })
}

fn zero_resources_summary() -> Value {
    json!({
        "resourcesProcessed": 0,
        "resourcesFlagged": 0,
        "resourcesIgnored": 0,
        "resourcesSuppressed": 0,
    })
}

fn category_specific_summary(check_id: &str) -> Value {
    // The cost-optimising summary is only meaningful for cost checks, but the
    // member is always present in the live response with zeroed savings.
    let _ = catalog::check_category(check_id);
    json!({
        "costOptimizing": {
            "estimatedMonthlySavings": 0.0,
            "estimatedPercentMonthlySavings": 0.0,
        }
    })
}

/// Milliseconds until the check may be refreshed again. Zero while a refresh is
/// in flight; a full interval once it has settled.
fn refresh_interval_ms(status: &str) -> i64 {
    match status {
        "success" | "none" => 3_600_000,
        _ => 0,
    }
}

// ---- pagination helpers ---------------------------------------------------

/// Decode a `nextToken` (a plain decimal offset) into a start index.
fn decode_token(token: Option<&str>) -> usize {
    token.and_then(|t| t.parse::<usize>().ok()).unwrap_or(0)
}

/// Return `(page, next_token)` for `items[start..]` limited to `max`.
fn paginate(items: &[Value], start: usize, max: Option<usize>) -> (Vec<Value>, Option<String>) {
    let start = start.min(items.len());
    let remaining = &items[start..];
    match max {
        Some(m) if m < remaining.len() => {
            let page = remaining[..m].to_vec();
            (page, Some((start + m).to_string()))
        }
        _ => (remaining.to_vec(), None),
    }
}

/// Summarise an attachment set (stored as `{attachmentIds, expiryTime}`) into
/// the `AttachmentSet` wire list of `{attachmentId, fileName}`.
fn attachment_set_summary(d: &SupportData, set: &Value) -> Value {
    let ids = set
        .get("attachmentIds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let details: Vec<Value> = ids
        .iter()
        .filter_map(|v| v.as_str())
        .map(|id| {
            let file_name = d
                .attachments
                .get(id)
                .and_then(|a| a.get("fileName"))
                .cloned()
                .unwrap_or(json!(""));
            json!({ "attachmentId": id, "fileName": file_name })
        })
        .collect();
    json!(details)
}

/// The endpoint presigned attachment links should point at: the endpoint this
/// server was started with, falling back to the request's own `Host` (a
/// snapshot written before the upload flow existed carries no endpoint).
fn link_endpoint(req: &AwsRequest, d: &SupportData) -> String {
    if !d.endpoint.is_empty() {
        return d.endpoint.clone();
    }
    req.headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|host| format!("http://{host}"))
        .unwrap_or_else(|| "http://localhost:4566".to_string())
}

/// The region to name in a presigned link's credential scope.
fn link_region(req: &AwsRequest, d: &SupportData) -> String {
    if !d.region.is_empty() {
        d.region.clone()
    } else {
        req.region.clone()
    }
}

/// Resolve the half-open `uploadRange` a `GetAttachmentUploadLinks` call asked
/// for into the exclusive end index to hand links out up to. `startIndex` is
/// inclusive and `endIndex` exclusive, the range covers at most
/// [`MAX_UPLOAD_URLS_PER_CALL`] parts, and an end past the last part is capped
/// there rather than refused (the file simply has fewer parts left).
fn validate_upload_range(
    upload_id: &str,
    start: i64,
    end: i64,
    total_parts: i64,
) -> Result<i64, AwsServiceError> {
    if start < 1 {
        return Err(validation_error(
            "uploadRange.startIndex must be at least 1; part indexes start at 1.",
        ));
    }
    if start > total_parts {
        return Err(validation_error(format!(
            "uploadRange.startIndex {start} is past the last part of upload {upload_id}, which has {total_parts} parts."
        )));
    }
    if end <= start {
        return Err(validation_error(format!(
            "uploadRange.endIndex {end} must be greater than startIndex {start}; endIndex is exclusive."
        )));
    }
    if end - start > MAX_UPLOAD_URLS_PER_CALL {
        return Err(validation_error(format!(
            "uploadRange size (endIndex - startIndex) must not exceed {MAX_UPLOAD_URLS_PER_CALL}."
        )));
    }
    Ok(end.min(total_parts + 1))
}

/// Concatenate an upload's parts into the file they make up. Every part must
/// still hold its bytes, and the assembled length must be the `fileSizeBytes`
/// the upload declared (an upload that declared none has nothing to check).
fn assemble_parts(
    upload_id: &str,
    parts: &[UploadPart],
    total_parts: i64,
    file_size_bytes: i64,
) -> Result<Vec<u8>, AwsServiceError> {
    let mut bytes: Vec<u8> = Vec::new();
    for part_index in 1..=total_parts {
        let part = parts
            .iter()
            .find(|p| p.part_index == part_index)
            .ok_or_else(|| {
                validation_error(format!(
                    "No upload link was issued for part {part_index} of upload {upload_id}."
                ))
            })?;
        let Some(data) = &part.data else {
            return Err(validation_error(format!(
                "Part {part_index} of upload {upload_id} was never uploaded."
            )));
        };
        let mut decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|err| {
                internal_server_error(format!(
                    "part {part_index} of upload {upload_id} could not be decoded: {err}"
                ))
            })?;
        bytes.append(&mut decoded);
    }
    if file_size_bytes > 0 && bytes.len() as i64 != file_size_bytes {
        return Err(validation_error(format!(
            "The parts of upload {upload_id} assemble to {} bytes, but the upload declared {file_size_bytes} bytes.",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Compare an `ETag` the client echoed back against the one the data plane
/// issued. Clients quote it, strip the quotes, or (via XML-encoding layers in
/// some SDKs and Terraform) send the quotes as numeric entities, so normalise
/// all of those away before comparing.
fn etags_match(stored: &str, claimed: &str) -> bool {
    normalize_etag(stored) == normalize_etag(claimed)
}

fn normalize_etag(tag: &str) -> String {
    tag.replace("&quot;", "")
        .replace("&#34;", "")
        .replace("&#x22;", "")
        .replace('"', "")
        .trim()
        .to_ascii_lowercase()
}

/// The `attachments` member of a communication: the attachment set's entries
/// plus anything attached through `uploadIds`. The model added `attachments`
/// alongside the older `attachmentSet` so large, upload-based attachments have
/// somewhere to appear; both members are populated so either client works.
fn communication_attachments(attachment_set: &Value, upload_details: Vec<Value>) -> Value {
    let mut all = attachment_set.as_array().cloned().unwrap_or_default();
    all.extend(upload_details);
    Value::Array(all)
}

/// Whether the request asked for a dry run.
fn is_dry_run(body: &Value) -> bool {
    body.get("dryRun").and_then(Value::as_bool).unwrap_or(false)
}

/// Resolve `uploadIds` to the attachments their completed uploads produced.
/// Neither `CreateCase` nor `AddCommunicationToCase` models `UploadIdNotFound`,
/// so an unknown or unfinished upload is reported as a request-validation
/// failure, which both operations can express.
fn uploads_to_attachment_details(
    d: &SupportData,
    upload_ids: &[String],
) -> Result<Vec<Value>, AwsServiceError> {
    let mut details = Vec::with_capacity(upload_ids.len());
    for upload_id in upload_ids {
        let upload = d
            .attachment_uploads
            .get(upload_id)
            .ok_or_else(|| validation_error(format!("Upload {upload_id} was not found.")))?;
        let attachment_id = upload.attachment_id.as_ref().ok_or_else(|| {
            validation_error(format!(
                "Upload {upload_id} has not been completed; call CompleteAttachmentUpload first."
            ))
        })?;
        details.push(json!({
            "attachmentId": attachment_id,
            "fileName": upload.file_name,
        }));
    }
    Ok(details)
}

fn string_list(body: &Value, name: &str) -> Vec<String> {
    body.get(name)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

// ---- error / response helpers --------------------------------------------

fn validation_error(msg: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(StatusCode::BAD_REQUEST, "ValidationException", msg.into())
}

fn case_id_not_found(id: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "CaseIdNotFound",
        format!("Case {id} was not found."),
    )
}

fn attachment_id_not_found(id: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "AttachmentIdNotFound",
        format!("Attachment {id} was not found."),
    )
}

fn attachment_set_id_not_found(id: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "AttachmentSetIdNotFound",
        format!("Attachment set {id} was not found."),
    )
}

/// `UploadIdNotFound` is the only upload-lookup error the three upload
/// operations model, so it carries every reason an upload id cannot be acted
/// on; the message says which.
fn upload_id_not_found(id: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "UploadIdNotFound",
        format!("Upload {id} was not found."),
    )
}

fn upload_already_completed(id: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "UploadIdNotFound",
        format!("Upload {id} has already been completed."),
    )
}

fn upload_expired(id: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "UploadIdNotFound",
        format!("Upload {id} has expired."),
    )
}

fn dry_run_error(action: &str) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::BAD_REQUEST,
        "DryRunOperationException",
        format!("Request would have succeeded, but DryRun flag is set for {action}."),
    )
}

fn internal_server_error(msg: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "InternalServerError",
        msg.into(),
    )
}

fn ok(value: Value) -> AwsResponse {
    AwsResponse::ok_json(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fakecloud_core::multi_account::MultiAccountState;
    use parking_lot::RwLock;

    fn service() -> SupportService {
        SupportService::new(Arc::new(RwLock::new(MultiAccountState::new(
            "000000000000",
            "us-east-1",
            "",
        ))))
    }

    fn req(action: &str, body: Value) -> AwsRequest {
        AwsRequest {
            service: "support".to_string(),
            action: action.to_string(),
            region: "us-east-1".to_string(),
            account_id: "000000000000".to_string(),
            request_id: "test".to_string(),
            headers: http::HeaderMap::new(),
            query_params: std::collections::HashMap::new(),
            body: bytes::Bytes::from(serde_json::to_vec(&body).unwrap()),
            body_stream: parking_lot::Mutex::new(None),
            path_segments: Vec::new(),
            raw_path: String::new(),
            raw_query: String::new(),
            method: http::Method::POST,
            is_query_protocol: false,
            access_key_id: None,
            principal: None,
        }
    }

    fn body_of(resp: &AwsResponse) -> Value {
        serde_json::from_slice(resp.body.expect_bytes()).unwrap()
    }

    /// `Result::unwrap_err` requires the `Ok` type to be `Debug`; `AwsResponse`
    /// is not, so extract the error by matching instead.
    fn expect_err(r: Result<AwsResponse, AwsServiceError>) -> AwsServiceError {
        match r {
            Ok(_) => panic!("expected an error response"),
            Err(e) => e,
        }
    }

    #[test]
    fn case_lifecycle() {
        let svc = service();
        // Create.
        let created = svc
            .create_case(
                &req("CreateCase", json!({})),
                &json!({ "subject": "Cannot connect", "communicationBody": "help please" }),
            )
            .unwrap();
        let case_id = body_of(&created)["caseId"].as_str().unwrap().to_string();
        assert!(case_id.starts_with("case-000000000000-"));

        // Describe returns the case with its seeded communication.
        let described = svc
            .describe_cases(&req("DescribeCases", json!({})), &json!({}))
            .unwrap();
        let out = body_of(&described);
        assert_eq!(out["cases"].as_array().unwrap().len(), 1);
        let comms = &out["cases"][0]["recentCommunications"]["communications"];
        assert_eq!(comms.as_array().unwrap().len(), 1);
        assert_eq!(comms[0]["body"], "help please");

        // Add a communication.
        let added = svc
            .add_communication_to_case(
                &req("AddCommunicationToCase", json!({})),
                &json!({ "caseId": case_id, "communicationBody": "any update?" }),
            )
            .unwrap();
        assert_eq!(body_of(&added)["result"], true);
        let comms = svc
            .describe_communications(
                &req("DescribeCommunications", json!({})),
                &json!({ "caseId": case_id }),
            )
            .unwrap();
        assert_eq!(
            body_of(&comms)["communications"].as_array().unwrap().len(),
            2
        );

        // Resolve.
        let resolved = svc
            .resolve_case(
                &req("ResolveCase", json!({})),
                &json!({ "caseId": case_id }),
            )
            .unwrap();
        let out = body_of(&resolved);
        assert_eq!(out["initialCaseStatus"], "opened");
        assert_eq!(out["finalCaseStatus"], "resolved");

        // Resolved cases are excluded unless includeResolvedCases.
        let default_list = svc
            .describe_cases(&req("DescribeCases", json!({})), &json!({}))
            .unwrap();
        assert_eq!(body_of(&default_list)["cases"].as_array().unwrap().len(), 0);
        let with_resolved = svc
            .describe_cases(
                &req("DescribeCases", json!({})),
                &json!({ "includeResolvedCases": true }),
            )
            .unwrap();
        assert_eq!(
            body_of(&with_resolved)["cases"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn unknown_case_is_case_id_not_found() {
        let svc = service();
        let err = expect_err(svc.describe_communications(
            &req("DescribeCommunications", json!({})),
            &json!({ "caseId": "case-nope" }),
        ));
        assert!(format!("{err:?}").contains("CaseIdNotFound"));
    }

    #[test]
    fn attachment_set_lifecycle() {
        let svc = service();
        let created = svc
            .add_attachments_to_set(
                &req("AddAttachmentsToSet", json!({})),
                &json!({ "attachments": [{ "fileName": "log.txt", "data": "aGVsbG8=" }] }),
            )
            .unwrap();
        let out = body_of(&created);
        let set_id = out["attachmentSetId"].as_str().unwrap().to_string();
        assert!(set_id.starts_with("as-"));
        assert!(out["expiryTime"].is_string());

        // Extend the same set.
        let extended = svc
            .add_attachments_to_set(
                &req("AddAttachmentsToSet", json!({})),
                &json!({ "attachmentSetId": set_id, "attachments": [{ "fileName": "b.txt", "data": "eA==" }] }),
            )
            .unwrap();
        assert_eq!(body_of(&extended)["attachmentSetId"], set_id);

        // Unknown set id is rejected.
        let err = expect_err(svc.add_attachments_to_set(
            &req("AddAttachmentsToSet", json!({})),
            &json!({ "attachmentSetId": "as-missing", "attachments": [] }),
        ));
        assert!(format!("{err:?}").contains("AttachmentSetIdNotFound"));
    }

    #[test]
    fn describe_attachment_unknown_id() {
        let svc = service();
        let err = expect_err(svc.describe_attachment(
            &req("DescribeAttachment", json!({})),
            &json!({ "attachmentId": "attachment-x" }),
        ));
        assert!(format!("{err:?}").contains("AttachmentIdNotFound"));
    }

    #[test]
    fn severity_levels_and_ta_catalogue() {
        let svc = service();
        let sev = body_of(&svc.describe_severity_levels(&json!({})).unwrap());
        assert_eq!(sev["severityLevels"].as_array().unwrap().len(), 5);
        let checks = body_of(&svc.describe_ta_checks(&json!({})).unwrap());
        assert!(!checks["checks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn ta_refresh_state_machine() {
        let svc = service();
        // Enqueue.
        let enq = body_of(
            &svc.refresh_ta_check(
                &req("RefreshTrustedAdvisorCheck", json!({})),
                &json!({ "checkId": "Qch7DwouX1" }),
            )
            .unwrap(),
        );
        assert_eq!(enq["status"]["status"], "enqueued");
        // Each describe advances: enqueued -> processing -> success.
        let step1 = body_of(
            &svc.describe_ta_refresh_statuses(
                &req("DescribeTrustedAdvisorCheckRefreshStatuses", json!({})),
                &json!({ "checkIds": ["Qch7DwouX1"] }),
            )
            .unwrap(),
        );
        assert_eq!(step1["statuses"][0]["status"], "processing");
        let step2 = body_of(
            &svc.describe_ta_refresh_statuses(
                &req("DescribeTrustedAdvisorCheckRefreshStatuses", json!({})),
                &json!({ "checkIds": ["Qch7DwouX1"] }),
            )
            .unwrap(),
        );
        assert_eq!(step2["statuses"][0]["status"], "success");
    }

    /// Drive the real upload data plane for `body`'s worth of bytes and return
    /// `(uploadId, completedUploads)` ready for `CompleteAttachmentUpload`.
    fn upload_one_part(svc: &SupportService, file_name: &str, bytes: &[u8]) -> (String, Value) {
        let links = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({ "fileName": file_name, "fileSizeBytes": bytes.len() }),
            )
            .unwrap(),
        );
        let upload_id = links["uploadId"].as_str().unwrap().to_string();
        let signature = signature_of(links["uploadUrls"][0]["url"].as_str().unwrap());
        let outcome = crate::dataplane::put_upload_part(
            &svc.state,
            "000000000000",
            &upload_id,
            1,
            &signature,
            bytes,
        );
        let etag = match outcome {
            crate::dataplane::PutPartOutcome::Stored(tag) => tag,
            other => panic!("expected the part to be stored, got {other:?}"),
        };
        (upload_id, json!([{ "partIndex": 1, "eTag": etag }]))
    }

    /// Pull `X-Amz-Signature` out of a presigned URL.
    fn signature_of(url: &str) -> String {
        url.rsplit("X-Amz-Signature=")
            .next()
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string()
    }

    #[test]
    fn attachment_upload_round_trips_through_the_data_plane() {
        let svc = service();
        let links = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({ "fileName": "log.txt", "fileSizeBytes": 5 }),
            )
            .unwrap(),
        );
        assert!(links["uploadId"].as_str().unwrap().starts_with("upload-"));
        assert_eq!(links["partSizeBytes"], 5 * 1024 * 1024);
        assert_eq!(links["totalParts"], 1);
        // The only part's URL was returned, so there is no next page to ask
        // for.
        assert!(links["nextIndex"].is_null(), "{links}");
        let url = links["uploadUrls"][0]["url"].as_str().unwrap();
        assert!(url.contains("/_fakecloud/support/attachments/uploads/000000000000/"));
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(links["uploadUrls"][0]["expiryDate"].is_string());
        assert_eq!(links["uploadUrls"][0]["partIndex"], 1);

        // Upload the bytes over the presigned link, then complete.
        let upload_id = links["uploadId"].as_str().unwrap().to_string();
        let signature = signature_of(url);
        let etag = match crate::dataplane::put_upload_part(
            &svc.state,
            "000000000000",
            &upload_id,
            1,
            &signature,
            b"hello",
        ) {
            crate::dataplane::PutPartOutcome::Stored(tag) => tag,
            other => panic!("expected the part to be stored, got {other:?}"),
        };

        // Progress is visible before completion.
        let status = body_of(
            &svc.describe_attachment_upload_status(
                &req("DescribeAttachmentUploadStatus", json!({})),
                &json!({ "uploadId": upload_id }),
            )
            .unwrap(),
        );
        assert_eq!(status["uploadStatus"], "attachment-not-ready");
        assert_eq!(status["fileName"], "log.txt");
        assert_eq!(status["uploadProgress"]["totalParts"], 1);
        assert_eq!(status["uploadProgress"]["completedPartsCount"], 1);

        let completed = body_of(
            &svc.complete_attachment_upload(
                &req("CompleteAttachmentUpload", json!({})),
                &json!({
                    "uploadId": upload_id,
                    "completedUploads": [{ "partIndex": 1, "eTag": etag }],
                }),
            )
            .unwrap(),
        );
        assert_eq!(completed["uploadStatus"], "attachment-ready");

        let status = body_of(
            &svc.describe_attachment_upload_status(
                &req("DescribeAttachmentUploadStatus", json!({})),
                &json!({ "uploadId": upload_id }),
            )
            .unwrap(),
        );
        assert_eq!(status["uploadStatus"], "attachment-ready");

        // The assembled attachment is retrievable and downloadable.
        let attachment_id = svc
            .state
            .read()
            .get("000000000000")
            .unwrap()
            .attachment_uploads[&upload_id]
            .attachment_id
            .clone()
            .unwrap();
        let described = body_of(
            &svc.describe_attachment(
                &req("DescribeAttachment", json!({})),
                &json!({ "attachmentId": attachment_id }),
            )
            .unwrap(),
        );
        assert_eq!(described["attachment"]["fileName"], "log.txt");
        assert_eq!(described["attachment"]["data"], "aGVsbG8=");

        let link = body_of(
            &svc.get_attachment_download_link(
                &req("GetAttachmentDownloadLink", json!({})),
                &json!({ "attachmentId": attachment_id }),
            )
            .unwrap(),
        );
        assert_eq!(link["fileName"], "log.txt");
        let download_url = link["downloadUrl"]["url"].as_str().unwrap();
        assert!(download_url.contains(&format!(
            "/_fakecloud/support/attachments/downloads/000000000000/{attachment_id}"
        )));
        assert!(link["downloadUrl"]["expiryDate"].is_string());
        assert_eq!(
            crate::dataplane::fetch_attachment(
                &svc.state,
                "000000000000",
                &attachment_id,
                &signature_of(download_url),
            ),
            crate::dataplane::DownloadOutcome::Found("log.txt".into(), b"hello".to_vec()),
        );
    }

    /// The part indexes a `GetAttachmentUploadLinks` response handed out.
    fn issued_parts(links: &Value) -> Vec<i64> {
        links["uploadUrls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["partIndex"].as_i64().unwrap())
            .collect()
    }

    #[test]
    fn multipart_upload_splits_into_five_mib_parts() {
        let svc = service();
        let links = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({ "fileName": "big.bin", "fileSizeBytes": 12 * 1024 * 1024 }),
            )
            .unwrap(),
        );
        assert_eq!(links["totalParts"], 3);
        assert_eq!(issued_parts(&links), vec![1, 2, 3]);
        assert!(links["nextIndex"].is_null(), "{links}");

        // A resume asks for a sub-range of the same upload. `endIndex` is
        // exclusive, so 2..4 is parts 2 and 3.
        let upload_id = links["uploadId"].as_str().unwrap().to_string();
        let resumed = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({
                    "fileName": "big.bin",
                    "uploadId": upload_id,
                    "uploadRange": { "startIndex": 2, "endIndex": 4 },
                }),
            )
            .unwrap(),
        );
        assert_eq!(resumed["uploadId"], upload_id);
        assert_eq!(issued_parts(&resumed), vec![2, 3]);
        assert!(resumed["nextIndex"].is_null(), "{resumed}");

        // A one-part range is startIndex..startIndex + 1.
        let single = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({
                    "fileName": "big.bin",
                    "uploadId": upload_id,
                    "uploadRange": { "startIndex": 2, "endIndex": 3 },
                }),
            )
            .unwrap(),
        );
        assert_eq!(issued_parts(&single), vec![2]);
        // Part 3 still has no URL from this call, so that is where the caller
        // picks up.
        assert_eq!(single["nextIndex"], 3, "{single}");
    }

    #[test]
    fn upload_links_are_paged_ten_at_a_time() {
        let svc = service();
        // 25 parts: far more than one call may hand out.
        let links = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({ "fileName": "huge.bin", "fileSizeBytes": 121 * 1024 * 1024 }),
            )
            .unwrap(),
        );
        assert_eq!(links["totalParts"], 25);
        assert_eq!(issued_parts(&links), (1..=10).collect::<Vec<i64>>());
        assert_eq!(links["nextIndex"], 11, "{links}");

        // Paging with the returned nextIndex walks the rest of the file.
        let upload_id = links["uploadId"].as_str().unwrap().to_string();
        let mut next = links["nextIndex"].as_i64().unwrap();
        let mut seen: Vec<i64> = (1..=10).collect();
        while next != 0 {
            let page = body_of(
                &svc.get_attachment_upload_links(
                    &req("GetAttachmentUploadLinks", json!({})),
                    &json!({
                        "fileName": "huge.bin",
                        "uploadId": upload_id,
                        "uploadRange": { "startIndex": next },
                    }),
                )
                .unwrap(),
            );
            seen.extend(issued_parts(&page));
            next = page["nextIndex"].as_i64().unwrap_or(0);
        }
        assert_eq!(seen, (1..=25).collect::<Vec<i64>>());
    }

    #[test]
    fn upload_range_is_validated() {
        let svc = service();
        let links = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({ "fileName": "huge.bin", "fileSizeBytes": 121 * 1024 * 1024 }),
            )
            .unwrap(),
        );
        let upload_id = links["uploadId"].as_str().unwrap().to_string();
        let range = |start: i64, end: i64| {
            json!({
                "fileName": "huge.bin",
                "uploadId": upload_id,
                "uploadRange": { "startIndex": start, "endIndex": end },
            })
        };

        // More than ten URLs in one call.
        let err = expect_err(svc.get_attachment_upload_links(
            &req("GetAttachmentUploadLinks", json!({})),
            &range(1, 12),
        ));
        assert!(format!("{err:?}").contains("must not exceed 10"), "{err:?}");

        // An empty or inverted half-open range.
        let err = expect_err(svc.get_attachment_upload_links(
            &req("GetAttachmentUploadLinks", json!({})),
            &range(3, 3),
        ));
        assert!(
            format!("{err:?}").contains("must be greater than startIndex"),
            "{err:?}"
        );

        // A start past the last part.
        let err = expect_err(svc.get_attachment_upload_links(
            &req("GetAttachmentUploadLinks", json!({})),
            &range(26, 27),
        ));
        assert!(format!("{err:?}").contains("past the last part"), "{err:?}");

        // Exactly ten is allowed, and an end past the last part is capped
        // there rather than refused.
        let ok_page = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &range(16, 26),
            )
            .unwrap(),
        );
        assert_eq!(issued_parts(&ok_page), (16..=25).collect::<Vec<i64>>());
        assert!(ok_page["nextIndex"].is_null(), "{ok_page}");
        let capped = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &range(24, 34),
            )
            .unwrap(),
        );
        assert_eq!(issued_parts(&capped), vec![24, 25]);
    }

    #[test]
    fn reissued_links_keep_the_signature_and_the_upload_deadline() {
        let svc = service();
        let links = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({ "fileName": "big.bin", "fileSizeBytes": 6 * 1024 * 1024 }),
            )
            .unwrap(),
        );
        let upload_id = links["uploadId"].as_str().unwrap().to_string();
        let first_signature = signature_of(links["uploadUrls"][0]["url"].as_str().unwrap());
        let deadline = svc
            .state
            .read()
            .get("000000000000")
            .unwrap()
            .attachment_uploads[&upload_id]
            .expiry
            .clone();

        let again = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({ "fileName": "big.bin", "uploadId": upload_id }),
            )
            .unwrap(),
        );
        // The URL the caller may already be uploading to still works: the
        // signature was reused rather than rotated.
        assert_eq!(
            signature_of(again["uploadUrls"][0]["url"].as_str().unwrap()),
            first_signature
        );
        // And asking again does not push the upload's own deadline back.
        assert_eq!(
            svc.state
                .read()
                .get("000000000000")
                .unwrap()
                .attachment_uploads[&upload_id]
                .expiry,
            deadline
        );

        // A part whose link expired does get a fresh signature, but never one
        // that outlives the upload.
        svc.state
            .write()
            .get_mut("000000000000")
            .unwrap()
            .attachment_uploads
            .get_mut(&upload_id)
            .unwrap()
            .part_mut(1)
            .unwrap()
            .expiry = "2000-01-01T00:00:00.000Z".to_string();
        let rotated = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({ "fileName": "big.bin", "uploadId": upload_id }),
            )
            .unwrap(),
        );
        assert_ne!(
            signature_of(rotated["uploadUrls"][0]["url"].as_str().unwrap()),
            first_signature
        );
        let guard = svc.state.read();
        let upload = &guard.get("000000000000").unwrap().attachment_uploads[&upload_id];
        assert!(upload.part(1).unwrap().expiry <= upload.expiry);
    }

    #[test]
    fn unknown_upload_id_is_upload_id_not_found() {
        let svc = service();
        for result in [
            svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({ "fileName": "log.txt", "uploadId": "upload-missing" }),
            ),
            svc.complete_attachment_upload(
                &req("CompleteAttachmentUpload", json!({})),
                &json!({ "uploadId": "upload-missing", "completedUploads": [] }),
            ),
            svc.describe_attachment_upload_status(
                &req("DescribeAttachmentUploadStatus", json!({})),
                &json!({ "uploadId": "upload-missing" }),
            ),
        ] {
            let err = expect_err(result);
            assert!(format!("{err:?}").contains("UploadIdNotFound"));
        }
    }

    #[test]
    fn completing_twice_is_rejected() {
        let svc = service();
        let (upload_id, completed_uploads) = upload_one_part(&svc, "log.txt", b"hello");
        let args = json!({ "uploadId": upload_id, "completedUploads": completed_uploads });
        svc.complete_attachment_upload(&req("CompleteAttachmentUpload", json!({})), &args)
            .unwrap();
        let err = expect_err(
            svc.complete_attachment_upload(&req("CompleteAttachmentUpload", json!({})), &args),
        );
        let rendered = format!("{err:?}");
        assert!(rendered.contains("UploadIdNotFound"), "{rendered}");
        assert!(rendered.contains("already been completed"), "{rendered}");
        // Re-issuing links for a completed upload is refused too.
        let err = expect_err(svc.get_attachment_upload_links(
            &req("GetAttachmentUploadLinks", json!({})),
            &json!({ "fileName": "log.txt", "uploadId": upload_id }),
        ));
        assert!(format!("{err:?}").contains("UploadIdNotFound"));
    }

    #[test]
    fn completing_an_expired_upload_is_rejected() {
        let svc = service();
        let (upload_id, completed_uploads) = upload_one_part(&svc, "log.txt", b"hello");
        svc.state
            .write()
            .get_mut("000000000000")
            .unwrap()
            .attachment_uploads
            .get_mut(&upload_id)
            .unwrap()
            .expiry = "2000-01-01T00:00:00.000Z".to_string();
        let err = expect_err(svc.complete_attachment_upload(
            &req("CompleteAttachmentUpload", json!({})),
            &json!({ "uploadId": upload_id, "completedUploads": completed_uploads }),
        ));
        let rendered = format!("{err:?}");
        assert!(rendered.contains("UploadIdNotFound"), "{rendered}");
        assert!(rendered.contains("expired"), "{rendered}");
        // And the status read reports it as failed rather than pending.
        let status = body_of(
            &svc.describe_attachment_upload_status(
                &req("DescribeAttachmentUploadStatus", json!({})),
                &json!({ "uploadId": upload_id }),
            )
            .unwrap(),
        );
        assert_eq!(status["uploadStatus"], "failed");
    }

    #[test]
    fn completing_rejects_missing_parts_and_bad_etags() {
        let svc = service();
        let (upload_id, completed_uploads) = upload_one_part(&svc, "log.txt", b"hello");

        // Part uploaded, but the client claims nothing.
        let err = expect_err(svc.complete_attachment_upload(
            &req("CompleteAttachmentUpload", json!({})),
            &json!({ "uploadId": upload_id, "completedUploads": [] }),
        ));
        assert!(
            format!("{err:?}").contains("must name at least one part"),
            "{err:?}"
        );

        // Part index that was never issued.
        let err = expect_err(svc.complete_attachment_upload(
            &req("CompleteAttachmentUpload", json!({})),
            &json!({
                "uploadId": upload_id,
                "completedUploads": [{ "partIndex": 9, "eTag": "\"x\"" }],
            }),
        ));
        assert!(format!("{err:?}").contains("has no part 9"));

        // Right part, wrong eTag.
        let err = expect_err(svc.complete_attachment_upload(
            &req("CompleteAttachmentUpload", json!({})),
            &json!({
                "uploadId": upload_id,
                "completedUploads": [{ "partIndex": 1, "eTag": "\"deadbeef\"" }],
            }),
        ));
        assert!(format!("{err:?}").contains("does not match"));

        // The genuine eTag still works, including unquoted.
        let unquoted = completed_uploads[0]["eTag"]
            .as_str()
            .unwrap()
            .trim_matches('"')
            .to_string();
        let completed = body_of(
            &svc.complete_attachment_upload(
                &req("CompleteAttachmentUpload", json!({})),
                &json!({
                    "uploadId": upload_id,
                    "completedUploads": [{ "partIndex": 1, "eTag": unquoted }],
                }),
            )
            .unwrap(),
        );
        assert_eq!(completed["uploadStatus"], "attachment-ready");
    }

    /// Upload part `part_index` of `upload_id` over its presigned link,
    /// returning the `ETag` the data plane issued.
    fn put_part(svc: &SupportService, links: &Value, part_index: i64, bytes: &[u8]) -> String {
        let upload_id = links["uploadId"].as_str().unwrap();
        let url = links["uploadUrls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["partIndex"].as_i64() == Some(part_index))
            .expect("a link was issued for this part")["url"]
            .as_str()
            .unwrap();
        match crate::dataplane::put_upload_part(
            &svc.state,
            "000000000000",
            upload_id,
            part_index,
            &signature_of(url),
            bytes,
        ) {
            crate::dataplane::PutPartOutcome::Stored(tag) => tag,
            other => panic!("expected the part to be stored, got {other:?}"),
        }
    }

    #[test]
    fn completing_reports_parts_one_call_at_a_time() {
        let svc = service();
        // Two parts: a full 5 MiB one and a 1 MiB remainder.
        let first = vec![b'a'; 5 * 1024 * 1024];
        let second = vec![b'b'; 1024 * 1024];
        let links = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({
                    "fileName": "big.bin",
                    "fileSizeBytes": first.len() + second.len(),
                }),
            )
            .unwrap(),
        );
        let upload_id = links["uploadId"].as_str().unwrap().to_string();
        let first_etag = put_part(&svc, &links, 1, &first);

        // The model allows completing one part per call: reporting part 1
        // alone succeeds and leaves the upload pending.
        let pending = body_of(
            &svc.complete_attachment_upload(
                &req("CompleteAttachmentUpload", json!({})),
                &json!({
                    "uploadId": upload_id,
                    "completedUploads": [{ "partIndex": 1, "eTag": first_etag }],
                }),
            )
            .unwrap(),
        );
        assert_eq!(pending["uploadStatus"], "attachment-not-ready");
        assert!(svc
            .state
            .read()
            .get("000000000000")
            .unwrap()
            .attachment_uploads[&upload_id]
            .attachment_id
            .is_none());

        // Part 2 is still uploadable, and reporting it finishes the upload.
        let second_etag = put_part(&svc, &links, 2, &second);
        let completed = body_of(
            &svc.complete_attachment_upload(
                &req("CompleteAttachmentUpload", json!({})),
                &json!({
                    "uploadId": upload_id,
                    "completedUploads": [{ "partIndex": 2, "eTag": second_etag }],
                }),
            )
            .unwrap(),
        );
        assert_eq!(completed["uploadStatus"], "attachment-ready");

        let guard = svc.state.read();
        let data = guard.get("000000000000").unwrap();
        let upload = &data.attachment_uploads[&upload_id];
        // The assembled file is both parts, in order.
        let attachment_id = upload.attachment_id.clone().unwrap();
        let stored = base64::engine::general_purpose::STANDARD
            .decode(data.attachments[&attachment_id]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(stored.len(), first.len() + second.len());
        assert_eq!(stored[0], b'a');
        assert_eq!(stored[stored.len() - 1], b'b');
        // The part payloads are released once the attachment holds the bytes,
        // so the file is not kept twice in memory or in the snapshot.
        assert!(upload.parts.iter().all(|p| p.data.is_none()));
        // Progress still reads as complete even though the payloads are gone.
        assert_eq!(upload.completed_parts(), 2);
    }

    #[test]
    fn completing_a_part_that_was_never_uploaded_is_rejected() {
        let svc = service();
        let links = body_of(
            &svc.get_attachment_upload_links(
                &req("GetAttachmentUploadLinks", json!({})),
                &json!({ "fileName": "big.bin", "fileSizeBytes": 6 * 1024 * 1024 }),
            )
            .unwrap(),
        );
        let upload_id = links["uploadId"].as_str().unwrap().to_string();
        let err = expect_err(svc.complete_attachment_upload(
            &req("CompleteAttachmentUpload", json!({})),
            &json!({
                "uploadId": upload_id,
                "completedUploads": [{ "partIndex": 2, "eTag": "\"x\"" }],
            }),
        ));
        assert!(format!("{err:?}").contains("was never uploaded"), "{err:?}");
        // The rejected call recorded nothing.
        assert!(
            !svc.state
                .read()
                .get("000000000000")
                .unwrap()
                .attachment_uploads[&upload_id]
                .part(2)
                .unwrap()
                .completed
        );
    }

    #[test]
    fn assembling_a_corrupt_upload_keeps_the_parts() {
        let svc = service();
        let (upload_id, completed_uploads) = upload_one_part(&svc, "log.txt", b"hello");
        // Shorten the stored payload behind the data plane's back: the parts no
        // longer add up to the declared file size.
        svc.state
            .write()
            .get_mut("000000000000")
            .unwrap()
            .attachment_uploads
            .get_mut(&upload_id)
            .unwrap()
            .part_mut(1)
            .unwrap()
            .data = Some(base64::engine::general_purpose::STANDARD.encode(b"hi"));
        let err = expect_err(svc.complete_attachment_upload(
            &req("CompleteAttachmentUpload", json!({})),
            &json!({ "uploadId": upload_id, "completedUploads": completed_uploads }),
        ));
        assert!(format!("{err:?}").contains("declared 5 bytes"), "{err:?}");
        // The failed assembly did not drop the uploaded bytes.
        let guard = svc.state.read();
        let upload = &guard.get("000000000000").unwrap().attachment_uploads[&upload_id];
        assert_eq!(upload.parts.len(), 1);
        assert!(upload.part(1).unwrap().data.is_some());
        assert_eq!(upload.status, UPLOAD_NOT_READY);
    }

    #[test]
    fn download_links_prune_expired_grants() {
        let svc = service();
        let (upload_id, completed_uploads) = upload_one_part(&svc, "log.txt", b"hello");
        svc.complete_attachment_upload(
            &req("CompleteAttachmentUpload", json!({})),
            &json!({ "uploadId": upload_id, "completedUploads": completed_uploads }),
        )
        .unwrap();
        let attachment_id = svc
            .state
            .read()
            .get("000000000000")
            .unwrap()
            .attachment_uploads[&upload_id]
            .attachment_id
            .clone()
            .unwrap();

        let link = |svc: &SupportService| {
            svc.get_attachment_download_link(
                &req("GetAttachmentDownloadLink", json!({})),
                &json!({ "attachmentId": attachment_id }),
            )
            .unwrap()
        };
        link(&svc);
        link(&svc);
        assert_eq!(
            svc.state
                .read()
                .get("000000000000")
                .unwrap()
                .attachment_downloads
                .len(),
            2
        );

        // Age both grants out; the next link sweeps them instead of letting
        // them pile up for the lifetime of the process.
        for grant in svc
            .state
            .write()
            .get_mut("000000000000")
            .unwrap()
            .attachment_downloads
            .values_mut()
        {
            grant.expiry = "2000-01-01T00:00:00.000Z".to_string();
        }
        link(&svc);
        assert_eq!(
            svc.state
                .read()
                .get("000000000000")
                .unwrap()
                .attachment_downloads
                .len(),
            1
        );
    }

    #[test]
    fn download_link_for_unknown_attachment_is_attachment_id_not_found() {
        let svc = service();
        let err = expect_err(svc.get_attachment_download_link(
            &req("GetAttachmentDownloadLink", json!({})),
            &json!({ "attachmentId": "attachment-missing" }),
        ));
        assert!(format!("{err:?}").contains("AttachmentIdNotFound"));
    }

    #[test]
    fn completed_uploads_attach_to_a_case() {
        let svc = service();
        let (upload_id, completed_uploads) = upload_one_part(&svc, "log.txt", b"hello");
        svc.complete_attachment_upload(
            &req("CompleteAttachmentUpload", json!({})),
            &json!({ "uploadId": upload_id, "completedUploads": completed_uploads }),
        )
        .unwrap();

        let created = body_of(
            &svc.create_case(
                &req("CreateCase", json!({})),
                &json!({
                    "subject": "logs attached",
                    "communicationBody": "see attached",
                    "uploadIds": [upload_id],
                }),
            )
            .unwrap(),
        );
        let case_id = created["caseId"].as_str().unwrap().to_string();
        let comms = body_of(
            &svc.describe_communications(
                &req("DescribeCommunications", json!({})),
                &json!({ "caseId": case_id }),
            )
            .unwrap(),
        );
        let attachments = &comms["communications"][0]["attachments"];
        assert_eq!(attachments.as_array().unwrap().len(), 1);
        assert_eq!(attachments[0]["fileName"], "log.txt");

        // An upload that was never completed cannot be attached.
        let (pending, _) = upload_one_part(&svc, "pending.txt", b"x");
        let err = expect_err(svc.add_communication_to_case(
            &req("AddCommunicationToCase", json!({})),
            &json!({
                "caseId": case_id,
                "communicationBody": "one more",
                "uploadIds": [pending],
            }),
        ));
        assert!(format!("{err:?}").contains("has not been completed"));
    }

    #[tokio::test]
    async fn dry_run_refuses_without_touching_state() {
        let svc = service();
        let err = expect_err(
            svc.handle(req(
                "CreateCase",
                json!({
                    "subject": "s",
                    "communicationBody": "b",
                    "dryRun": true,
                }),
            ))
            .await,
        );
        assert!(format!("{err:?}").contains("DryRunOperationException"));
        assert!(svc
            .state
            .read()
            .get("000000000000")
            .unwrap()
            .cases
            .is_empty());

        // Trusted Advisor does not model the exception, so dryRun is ignored.
        let resp = svc
            .handle(req(
                "DescribeTrustedAdvisorChecks",
                json!({ "language": "en", "dryRun": true }),
            ))
            .await
            .unwrap();
        assert!(resp.status.is_success());
    }

    #[test]
    fn every_supported_action_dispatches() {
        let svc = service();
        for action in SUPPORT_ACTIONS {
            let result = svc.dispatch(action, &req(action, json!({})));
            // Every action must be routed; a missing dispatch arm surfaces as
            // the catch-all error rather than a domain error.
            if let Err(err) = result {
                assert!(
                    !matches!(err, AwsServiceError::ActionNotImplemented { .. }),
                    "{action} has no dispatch arm"
                );
            }
        }
    }

    #[test]
    fn describe_cases_paginates() {
        let svc = service();
        for _ in 0..3 {
            svc.create_case(
                &req("CreateCase", json!({})),
                &json!({ "subject": "s", "communicationBody": "b" }),
            )
            .unwrap();
        }
        let page1 = body_of(
            &svc.describe_cases(
                &req("DescribeCases", json!({})),
                &json!({ "maxResults": 2 }),
            )
            .unwrap(),
        );
        assert_eq!(page1["cases"].as_array().unwrap().len(), 2);
        let token = page1["nextToken"].as_str().unwrap();
        let page2 = body_of(
            &svc.describe_cases(
                &req("DescribeCases", json!({})),
                &json!({ "maxResults": 2, "nextToken": token }),
            )
            .unwrap(),
        );
        assert_eq!(page2["cases"].as_array().unwrap().len(), 1);
        assert!(page2.get("nextToken").is_none());
    }
}
