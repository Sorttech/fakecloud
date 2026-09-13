+++
title = "AWS Support"
description = "AWS Support on fakecloud: the full 20-operation surface -- support cases, communications, attachment sets, presigned attachment uploads and downloads, severity levels, the service/category catalogue, and the Trusted Advisor check catalogue + refresh state machine -- with account-partitioned persistence. The Trusted Advisor analysis engine and the live support agent are documented gaps."
weight = 77
+++

fakecloud implements **AWS Support** (`support`), the programmatic interface to
AWS Support cases and AWS Trusted Advisor. All **20 operations** from the AWS
Smithy model ship now, backed by account-partitioned state that persists across
restarts in persistent mode. The wire protocol is awsJson1.1 (x-amz-target
`AWSSupport_20130415.<Op>`), signing as `support`.

This is the **control plane**: every case, communication, attachment set, and
Trusted Advisor refresh status is real, validated, persisted state -- no stubbed
success responses. Requests are validated against the model's `required` /
`length` / `range` constraints before any handler runs, and an operation that
dereferences a case that does not exist returns Support's `CaseIdNotFound`; an
unknown attachment id returns `AttachmentIdNotFound`, an unknown attachment
set id returns `AttachmentSetIdNotFound`, and an unknown, expired, or
already-completed upload id returns `UploadIdNotFound`. Every operation that
models `DryRunOperationException` honours `dryRun`: the request is validated
and then refused without touching state.

## Supported features

### Support cases

- **`CreateCase`** mints an AWS-shaped case id (`case-{account}-{year}-{hex}`)
  and a numeric `displayId`, opens the case with `status: "opened"`, records the
  `subject` / `serviceCode` / `severityCode` / `categoryCode` /
  `ccEmailAddresses` / `language` / `issueType`, and seeds the case's
  communication thread with the initial `communicationBody`. A supplied
  `attachmentSetId` is resolved onto the first communication (an unknown set id
  returns `AttachmentSetIdNotFound`).
- **`DescribeCases`** filters by `caseIdList` (an unknown id returns
  `CaseIdNotFound`), `displayId`, `afterTime` / `beforeTime`,
  `includeResolvedCases` (resolved cases are hidden by default), and
  `includeCommunications` (the recent-communications thread is embedded by
  default), and paginates with a round-tripping `nextToken`.
- **`AddCommunicationToCase`** appends a communication to the thread and returns
  `result: true`.
- **`DescribeCommunications`** returns the case's communication thread, filtered
  by time window and paginated with a `nextToken`.
- **`ResolveCase`** flips the case to `resolved` and returns both the
  `initialCaseStatus` and the `finalCaseStatus`.

### Attachment sets

- **`AddAttachmentsToSet`** mints a new `attachmentSetId` (or extends an existing
  one) and returns it with an `expiryTime` one hour out. Each attachment is
  stored under a generated `attachmentId`.
- **`DescribeAttachment`** returns a stored attachment (`fileName` + base64
  `data`) by id.

### Presigned attachment uploads

Large attachments do not travel in the API request. AWS hands out presigned
links and the client transfers the bytes itself; fakecloud does the same, with
the links pointing back at fakecloud, which serves them.

- **`GetAttachmentUploadLinks`** records a new upload (or resumes one named by
  `uploadId`, optionally narrowed to an `uploadRange`) and returns an
  `uploadId`, the `partSizeBytes` (5 MiB), the `totalParts` derived from
  `fileSizeBytes`, the `nextIndex` to ask for next, and one presigned `PUT`
  `url` per part with its own `expiryDate`. At most ten links come back per
  call, `uploadRange.endIndex` is exclusive (`{1, 4}` is parts 1, 2 and 3) and
  a wider range is rejected, and `nextIndex` is `null` once the last part has
  been handed out. Re-asking for a part that already has a live link returns
  that same link rather than rotating its signature, and re-asking never
  extends the upload's own deadline. Each link carries its own signature,
  recorded in state; a link that was never issued, was tampered with, or has
  expired is refused.
- The links are real. `PUT` the part's bytes to the URL and fakecloud stores
  them and returns the part's `ETag`, exactly as an S3 part upload does. Every
  part but the last must be exactly `partSizeBytes`, and the last part the
  remainder of `fileSizeBytes`; a wrongly sized part is rejected.
- **`CompleteAttachmentUpload`** takes the `uploadId` and the
  `completedUploads` list of `{partIndex, eTag}`. It can be called one part at
  a time or with several parts at once: each named part must have been uploaded
  with an `ETag` matching what the `PUT` returned, and the upload stays
  `attachment-not-ready` until every part has been completed, at which point
  the parts are concatenated into a real attachment retrievable with
  `DescribeAttachment`. Completing twice, or completing an upload whose links
  expired, returns `UploadIdNotFound`.
- **`DescribeAttachmentUploadStatus`** reports the recorded `uploadStatus`
  (`attachment-not-ready` / `attachment-ready` / `failed`), the `fileName`, and
  the `uploadProgress` (`totalParts` + `completedPartsCount`).
- **`GetAttachmentDownloadLink`** mints a presigned `GET` link for a stored
  attachment and returns it with the `fileName` and an `expiryDate`. Following
  the link serves the attachment's bytes; an unknown attachment id returns
  `AttachmentIdNotFound`.
- A completed upload attaches to a case through `uploadIds` on `CreateCase` or
  `AddCommunicationToCase`; the resulting communication lists it under
  `attachments`.

### Severity levels + case-creation reference data

- **`DescribeSeverityLevels`** returns the five real severity levels (`low`,
  `normal`, `high`, `urgent`, `critical`).
- **`DescribeServices`** returns the support service/category catalogue,
  optionally filtered by `serviceCodeList`.
- **`DescribeCreateCaseOptions`** and **`DescribeSupportedLanguages`** return
  well-formed case-creation option and supported-language data.

### Trusted Advisor

- **`DescribeTrustedAdvisorChecks`** returns the vendored catalogue of the
  well-known Trusted Advisor checks (id / name / description / category /
  metadata column headers) across all five categories -- cost optimising,
  security, fault tolerance, performance, and service limits. This is faithful
  static AWS reference data, not fabricated findings.
- **`DescribeTrustedAdvisorCheckResult`** and
  **`DescribeTrustedAdvisorCheckSummaries`** return well-formed result / summary
  shapes (status, timestamp, `resourcesSummary`, `categorySpecificSummary`,
  `flaggedResources`) reporting an all-clear account (zero flagged resources).
- **`RefreshTrustedAdvisorCheck`** enqueues a refresh and
  **`DescribeTrustedAdvisorCheckRefreshStatuses`** advances the per-check state
  machine one step on each read: `none` -> `enqueued` -> `processing` ->
  `success`, with `millisUntilNextRefreshable`.

## Honest gap: no Trusted Advisor engine, no live support agent

AWS Support's value is a human/automated support agent working your case and the
Trusted Advisor analysis engine inspecting your account. fakecloud runs
**neither**. Cases are real records with a real communication thread, but no
automated agent reply is generated -- a case stays exactly as you and your own
`AddCommunicationToCase` calls leave it. Trusted Advisor returns the real check
**catalogue** and a real refresh **state machine**, but
`DescribeTrustedAdvisorCheckResult` / `DescribeTrustedAdvisorCheckSummaries`
report a structurally correct **all-clear** result (zero flagged resources)
rather than inspecting your account and inventing findings. Everything around
the analysis -- cases, communications, attachment sets, severity levels, the
service catalogue, the check catalogue, the refresh lifecycle, validation, and
persistence -- is faithful. If your workload needs the Support contract (open a
case, thread communications, resolve it; enumerate checks and drive a refresh),
fakecloud is a faithful stand-in; if it needs an actual agent reply or real
account findings, that is out of scope.

## Validation

Every request is validated against the Support Smithy model's `required` /
`length` / `range` constraints before any handler runs. (`@pattern` is not
enforced: the only patterned member, `serviceCode`, is taken by operations that
declare no generic validation exception, so surfacing a pattern rejection would
fall outside their Smithy error contract.) The declared exceptions
`CaseIdNotFound`, `AttachmentIdNotFound`, `AttachmentSetIdNotFound`,
`AttachmentSetExpired`, `AttachmentSetSizeLimitExceeded`,
`AttachmentLimitExceeded`, `DescribeAttachmentLimitExceeded`,
`CaseCreationLimitExceeded`, `UploadIdNotFound`, `DryRunOperationException`,
and `InternalServerError` model the service's error surface.

## Persistence

Support state is account-partitioned and, in persistent mode, snapshotted to
disk and restored on restart. Cases, communication threads, attachment sets,
individual attachments, in-flight attachment uploads (including the bytes
already uploaded to their presigned links), issued download grants, and Trusted
Advisor refresh statuses all survive a restart. An upload whose links expired
while the server was down is swept to `failed` on load rather than resurrected.
