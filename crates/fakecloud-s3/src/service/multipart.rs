use bytes::Bytes;
use chrono::Utc;
use http::{HeaderMap, StatusCode};

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use fakecloud_persistence::{BodyRef, BodySource};

use crate::persistence::mpu_init_snapshot;
use crate::state::{MultipartUpload, S3Object, UploadPart};

use md5::{Digest, Md5};

use super::{
    canned_acl_grants, compute_md5, extract_user_metadata, no_such_bucket, no_such_key,
    no_such_upload, parse_complete_multipart_xml, parse_url_encoded_tags, precondition_failed,
    resolve_object, s3_xml, xml_escape, ObjectLockSettings, S3Service,
};

/// The `Location` of a completed multipart object: its URL on the bucket's
/// regional virtual-hosted endpoint, under the region's partition DNS suffix
/// (`https://<bucket>.s3.<region>.amazonaws.com/<key>`, `.amazonaws.com.cn` in
/// China).
fn completion_location(bucket: &str, region: &str, key: &str) -> String {
    format!(
        "https://{}/{key}",
        fakecloud_aws::endpoint::s3_regional_domain_name(bucket, region)
    )
}

/// Build the `CompleteMultipartUploadResult` XML response for an object that
/// already exists — used by the idempotent re-completion paths (an upload that
/// was already completed by a prior or concurrent request). `checksum_xml` is
/// the optional pre-rendered `<ChecksumX>…</ChecksumX>` element, or `""`.
fn completion_xml_response(
    bucket: &str,
    region: &str,
    key: &str,
    etag: &str,
    checksum_xml: &str,
) -> AwsResponse {
    let location = xml_escape(&completion_location(bucket, region, key));
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <CompleteMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Location>{location}</Location>\
         <Bucket>{}</Bucket>\
         <Key>{}</Key>\
         <ETag>&quot;{}&quot;</ETag>\
         {checksum_xml}\
         </CompleteMultipartUploadResult>",
        xml_escape(bucket),
        xml_escape(key),
        xml_escape(etag),
    );
    AwsResponse {
        status: StatusCode::OK,
        content_type: "application/xml".to_string(),
        body: body.into(),
        headers: HeaderMap::new(),
    }
}

impl S3Service {
    pub(super) fn create_multipart_upload(
        &self,
        account_id: &str,
        req: &AwsRequest,
        bucket: &str,
        key: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let upload_id = uuid::Uuid::new_v4().to_string();
        let content_type = req
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let metadata = extract_user_metadata(&req.headers);
        let sys_header = |name: &str| {
            req.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };
        let cache_control = sys_header("cache-control");
        let content_disposition = sys_header("content-disposition");
        let content_language = sys_header("content-language");
        let expires = sys_header("expires");
        let storage_class = req
            .headers
            .get("x-amz-storage-class")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("STANDARD")
            .to_string();
        let sse_algorithm = req
            .headers
            .get("x-amz-server-side-encryption")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let sse_kms_key_id = req
            .headers
            .get("x-amz-server-side-encryption-aws-kms-key-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let tagging = req
            .headers
            .get("x-amz-tagging")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let write_acl = self.resolve_write_acl_headers(account_id, bucket, &req.headers)?;

        let checksum_algorithm = req
            .headers
            .get("x-amz-checksum-algorithm")
            .or_else(|| req.headers.get("x-amz-sdk-checksum-algorithm"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_uppercase());

        let mut accts = self.state.write();
        let state = accts.get_or_create(account_id);
        let b = state
            .buckets
            .get_mut(bucket)
            .ok_or_else(|| no_such_bucket(bucket))?;

        // Explicit lock headers are validated now, like PutObject does, and
        // kept on the upload; the bucket default retention is applied when
        // the upload completes and the object version is created.
        let lock = ObjectLockSettings::from_request(&req.headers, b.object_lock_config.as_deref())?;

        let acl_grants = write_acl
            .grants_for(&b.acl_owner_id)
            .unwrap_or_else(|| canned_acl_grants("private", &b.acl_owner_id));

        let upload = MultipartUpload {
            upload_id: upload_id.clone(),
            key: key.to_string(),
            initiated: Utc::now(),
            parts: std::collections::BTreeMap::new(),
            metadata,
            content_type,
            storage_class,
            sse_algorithm: sse_algorithm.clone(),
            sse_kms_key_id: sse_kms_key_id.clone(),
            tagging,
            acl_grants,
            checksum_algorithm,
            cache_control,
            content_disposition,
            content_language,
            expires,
            lock,
        };
        // Store-first: persist the MPU init before touching memory so a disk
        // write failure short-circuits the handler cleanly without leaving an
        // orphaned in-memory upload.
        let init_snapshot = mpu_init_snapshot(&upload);
        self.store
            .mpu_create(bucket, &upload_id, &init_snapshot)
            .map_err(super::persistence_error)?;
        b.multipart_uploads.insert(upload_id.clone(), upload);

        let mut headers = HeaderMap::new();
        if let Some(algo) = &sse_algorithm {
            if let Ok(val) = algo.parse() {
                headers.insert("x-amz-server-side-encryption", val);
            }
        }
        if let Some(kid) = &sse_kms_key_id {
            if let Ok(val) = kid.parse() {
                headers.insert("x-amz-server-side-encryption-aws-kms-key-id", val);
            }
        }

        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <InitiateMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Bucket>{}</Bucket>\
             <Key>{}</Key>\
             <UploadId>{}</UploadId>\
             </InitiateMultipartUploadResult>",
            xml_escape(bucket),
            xml_escape(key),
            xml_escape(&upload_id),
        );
        Ok(AwsResponse {
            status: StatusCode::OK,
            content_type: "application/xml".to_string(),
            body: body.into(),
            headers,
        })
    }

    pub(super) async fn upload_part(
        &self,
        account_id: &str,
        req: &AwsRequest,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: i64,
    ) -> Result<AwsResponse, AwsServiceError> {
        // Validate part number. An out-of-range partNumber is an invalid
        // argument (400 InvalidArgument), not a missing upload (404) — the
        // low end previously returned NoSuchUpload.
        if !(1..=10000).contains(&part_number) {
            return Err(AwsServiceError::aws_error_with_fields(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "Part number must be an integer between 1 and 10000, inclusive",
                vec![
                    ("ArgumentName".to_string(), "partNumber".to_string()),
                    ("ArgumentValue".to_string(), part_number.to_string()),
                ],
            ));
        }
        let pn = part_number as u32;

        // Pre-validate the bucket/upload/key under a short read guard
        // so a bogus request doesn't cause us to spool the whole body
        // to disk before returning the error.
        {
            let accts = self.state.read();
            let __empty = crate::state::S3State::new(account_id, "us-east-1");
            let state = accts.get(account_id).unwrap_or(&__empty);
            let b = state
                .buckets
                .get(bucket)
                .ok_or_else(|| no_such_bucket(bucket))?;
            let upload = b
                .multipart_uploads
                .get(upload_id)
                .ok_or_else(|| no_such_upload(upload_id))?;
            if upload.key != key {
                return Err(no_such_upload(upload_id));
            }
        }

        // UploadPart is streaming-only: spool chunks to a tempfile
        // while computing MD5 + size in constant memory, then hand
        // the file to the store as `BodySource::File`.
        let stream = req.take_body_stream().ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "MalformedRequestBody",
                "UploadPart requires a streaming request body",
            )
        })?;
        let spooled = fakecloud_core::service::spool_request_stream(
            stream,
            self.store.spool_dir().as_deref(),
            fakecloud_core::service::is_aws_chunked(&req.headers),
        )
        .await?;
        let part_size = spooled.size;
        let etag = spooled.md5_hex.clone();
        let body_source = BodySource::File(spooled.path);

        let mut accts = self.state.write();
        let state = accts.get_or_create(account_id);
        let b = state
            .buckets
            .get_mut(bucket)
            .ok_or_else(|| no_such_bucket(bucket))?;
        let upload = b
            .multipart_uploads
            .get_mut(upload_id)
            .ok_or_else(|| no_such_upload(upload_id))?;
        if upload.key != key {
            return Err(no_such_upload(upload_id));
        }

        // Store-first: durably write the part body before mutating memory.
        // The store returns the BodyRef the runtime should keep — `Disk`
        // for disk-mode (so the part body never lands in RAM beyond the
        // hyper frame buffer), `Memory` for memory-mode.
        let body_ref = self
            .store
            .mpu_put_part(bucket, upload_id, pn, body_source, &etag)
            .map_err(super::persistence_error)?;
        let part = UploadPart {
            part_number: pn,
            body: body_ref,
            etag: etag.clone(),
            size: part_size,
            last_modified: Utc::now(),
        };
        upload.parts.insert(pn, part);

        let mut headers = HeaderMap::new();
        headers.insert("etag", format!("\"{etag}\"").parse().unwrap());
        if let Some(algo) = &upload.sse_algorithm {
            headers.insert("x-amz-server-side-encryption", algo.parse().unwrap());
        }
        if let Some(kid) = &upload.sse_kms_key_id {
            headers.insert(
                "x-amz-server-side-encryption-aws-kms-key-id",
                kid.parse().unwrap(),
            );
        }
        Ok(AwsResponse {
            status: StatusCode::OK,
            content_type: "application/xml".to_string(),
            body: Bytes::new().into(),
            headers,
        })
    }

    pub(super) fn upload_part_copy(
        &self,
        account_id: &str,
        req: &AwsRequest,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_number: i64,
    ) -> Result<AwsResponse, AwsServiceError> {
        let copy_source = req
            .headers
            .get("x-amz-copy-source")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidArgument",
                    "x-amz-copy-source header is required",
                )
            })?;

        // Parsed by the same function the source authorization uses, so the
        // object copied is exactly the object authorized.
        let source = super::parse_copy_source(copy_source).ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "Invalid copy source format",
            )
        })?;
        let source_version_id = source.version_id.clone();
        let (src_bucket, src_key) = (source.bucket.as_str(), source.key.as_str());

        let copy_range = req
            .headers
            .get("x-amz-copy-source-range")
            .and_then(|v| v.to_str().ok());

        // Copy-source preconditions (x-amz-copy-source-if-*). These were never
        // read on the part-copy path, so a mismatched source still copied + 200'd.
        let cs_if_match = req
            .headers
            .get("x-amz-copy-source-if-match")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let cs_if_none_match = req
            .headers
            .get("x-amz-copy-source-if-none-match")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let cs_if_modified_since = req
            .headers
            .get("x-amz-copy-source-if-modified-since")
            .and_then(|v| v.to_str().ok())
            .and_then(super::parse_http_date);
        let cs_if_unmodified_since = req
            .headers
            .get("x-amz-copy-source-if-unmodified-since")
            .and_then(|v| v.to_str().ok())
            .and_then(super::parse_http_date);

        // Phase 1 (read guard): resolve the source, check preconditions and
        // that the upload exists, and open the source body. Nothing is read
        // yet: the source can be 5 GB, and the global S3 lock must not be held
        // across that IO.
        let src_handle = {
            let accts = self.state.read();
            let empty_state = crate::state::S3State::new(account_id, "us-east-1");
            let state = accts.get(account_id).unwrap_or(&empty_state);
            // The source bucket is read in the account that owns it, which is
            // not the destination's when copying out of another account's
            // bucket (authorized by s3:GetObject on the source at dispatch).
            let src_state = accts
                .find_account(|s| s.buckets.contains_key(src_bucket))
                .and_then(|owner| accts.get(owner))
                .unwrap_or(state);
            let sb = src_state
                .buckets
                .get(src_bucket)
                .ok_or_else(|| no_such_bucket(src_bucket))?;

            let src_obj = if let Some(ref vid) = source_version_id {
                resolve_object(sb, src_key, Some(vid))?
            } else {
                sb.objects
                    .get(src_key)
                    .ok_or_else(|| no_such_key(src_key))?
            };
            let precondition_412 = || {
                AwsServiceError::aws_error(
                    StatusCode::PRECONDITION_FAILED,
                    "PreconditionFailed",
                    "At least one of the pre-conditions you specified did not hold",
                )
            };
            let src_etag = format!("\"{}\"", src_obj.etag);
            if let Some(ref im) = cs_if_match {
                if !super::etag_matches(im, &src_etag) {
                    return Err(precondition_412());
                }
            }
            if let Some(ref inm) = cs_if_none_match {
                if super::etag_matches(inm, &src_etag) {
                    return Err(precondition_412());
                }
            }
            if let Some(since) = cs_if_unmodified_since {
                if src_obj.last_modified > since {
                    return Err(precondition_412());
                }
            }
            if let Some(since) = cs_if_modified_since {
                if src_obj.last_modified <= since {
                    return Err(precondition_412());
                }
            }
            let b = state
                .buckets
                .get(bucket)
                .ok_or_else(|| no_such_bucket(bucket))?;
            match b.multipart_uploads.get(upload_id) {
                Some(upload) if upload.key == key => {}
                _ => return Err(no_such_upload(upload_id)),
            }
            crate::body_io::ObjectBodyHandle::open(&src_obj.body).map_err(super::io_to_aws)?
        };

        // Phase 2 (no lock): resolve x-amz-copy-source-range against the
        // source size and read only that range, then stage the part next to
        // the store.
        let src_size = src_handle.size();
        let (offset, len) = if let Some(range_str) = copy_range {
            let range_part = range_str.strip_prefix("bytes=").unwrap_or(range_str);
            let invalid_range = || {
                AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidArgument",
                    format!(
                        "The x-amz-copy-source-range value \"{range_str}\" is not valid for \
                         the source object of size {src_size}."
                    ),
                )
            };
            if let Some((start_str, end_str)) = range_part.split_once('-') {
                // A copy-source-range over an empty object has no valid offsets, and a
                // malformed/out-of-bounds range must be rejected rather than reading past
                // the end of the source.
                let last = src_size.checked_sub(1).ok_or_else(invalid_range)?;
                let start: u64 = start_str.trim().parse().map_err(|_| invalid_range())?;
                let end: u64 = if end_str.trim().is_empty() {
                    last
                } else {
                    end_str.trim().parse().map_err(|_| invalid_range())?
                };
                if start > end || end > last {
                    return Err(invalid_range());
                }
                (start, end - start + 1)
            } else {
                (0, src_size)
            }
        } else {
            (0, src_size)
        };
        let src_data = super::run_blocking_io(|| src_handle.read_range(offset, len))
            .map_err(super::io_to_aws)?;

        let data_len = src_data.len() as u64;
        let etag = compute_md5(&src_data);
        let mut staged = super::run_blocking_io(|| {
            crate::body_io::SpooledBody::spool(Some(&*self.store), src_data)
        })
        .map_err(super::io_to_aws)?;

        // Phase 3 (write guard): commit the part. The upload may have been
        // aborted or completed meanwhile; the staged file is then discarded.
        let mut accts = self.state.write();
        let state = accts.get_or_create(account_id);
        let b = state
            .buckets
            .get_mut(bucket)
            .ok_or_else(|| no_such_bucket(bucket))?;
        let upload = b
            .multipart_uploads
            .get_mut(upload_id)
            .ok_or_else(|| no_such_upload(upload_id))?;
        if upload.key != key {
            return Err(no_such_upload(upload_id));
        }

        // Store-first: durably write before the in-memory part is visible,
        // and keep the BodyRef the store returns (a disk file in persistent
        // mode) instead of pinning the copied bytes in RAM.
        let body_ref = self
            .store
            .mpu_put_part(
                bucket,
                upload_id,
                part_number as u32,
                staged.source(),
                &etag,
            )
            .map_err(super::persistence_error)?;
        let part = UploadPart {
            part_number: part_number as u32,
            body: body_ref,
            etag: etag.clone(),
            size: data_len,
            last_modified: Utc::now(),
        };
        upload.parts.insert(part_number as u32, part);

        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <CopyPartResult>\
             <ETag>&quot;{etag}&quot;</ETag>\
             <LastModified>{}</LastModified>\
             </CopyPartResult>",
            Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
        );
        Ok(s3_xml(StatusCode::OK, body))
    }

    pub(super) fn complete_multipart_upload(
        &self,
        account_id: &str,
        req: &AwsRequest,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body_str = std::str::from_utf8(&req.body).unwrap_or("");
        let submitted_parts = parse_complete_multipart_xml(body_str);

        if submitted_parts.is_empty() {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "MalformedXML",
                "The XML you provided was not well-formed or did not validate against our published schema",
            ));
        }

        let if_none_match = req
            .headers
            .get("x-amz-if-none-match")
            .or_else(|| req.headers.get("if-none-match"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // Capture the upload + the bucket facts we need under a brief read
        // lock, then release it so the (potentially multi-GB) part assembly +
        // disk persist below runs OFF-LOCK instead of holding the global S3
        // write lock and serializing every other S3 operation server-wide
        // while it runs (bug-audit 2026-05-28, 4.7).
        let (upload, already_has_object, region, bucket_region, notification_config, acl_owner_id) = {
            let accts = self.state.read();
            let empty = crate::state::S3State::new(account_id, "us-east-1");
            let state = accts.get(account_id).unwrap_or(&empty);
            let b = state
                .buckets
                .get(bucket)
                .ok_or_else(|| no_such_bucket(bucket))?;
            let upload = match b.multipart_uploads.get(upload_id) {
                Some(u) => u.clone(),
                None => {
                    // Upload already completed - return existing object if it
                    // exists. IfNoneMatch does NOT apply to re-completions.
                    if let Some(obj) = b.objects.get(key) {
                        return Ok(completion_xml_response(
                            bucket, &b.region, key, &obj.etag, "",
                        ));
                    }
                    return Err(no_such_upload(upload_id));
                }
            };
            (
                upload,
                b.objects.contains_key(key),
                state.region.clone(),
                b.region.clone(),
                b.notification_config.clone(),
                b.acl_owner_id.clone(),
            )
        };

        if upload.key != key {
            return Err(no_such_upload(upload_id));
        }

        // IfNoneMatch: if "*" and object already exists, reject (only for real completions)
        if let Some(ref inm) = if_none_match {
            if inm == "*" && already_has_object {
                return Err(precondition_failed("If-None-Match"));
            }
        }

        // Validate parts are in ascending order
        for window in submitted_parts.windows(2) {
            if window[0].0 >= window[1].0 {
                return Err(AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidPartOrder",
                    "The list of parts was not in ascending order. The parts list must be specified in order by part number.",
                ));
            }
        }

        let sorted_parts = submitted_parts;

        // Validate all specified parts exist in the upload
        for (part_num, _) in &sorted_parts {
            if !upload.parts.contains_key(part_num) {
                return Err(AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidPart",
                    "One or more of the specified parts could not be found. The part may not have been uploaded, or the specified entity tag may not have matched the part's entity tag.",
                ));
            }
        }

        // Validate minimum part size: all non-last parts must be >= 5MB
        const MIN_PART_SIZE: usize = 5 * 1024 * 1024; // 5MB
        if sorted_parts.len() > 1 {
            for (i, (part_num, _)) in sorted_parts.iter().enumerate() {
                if i >= sorted_parts.len() - 1 {
                    break; // skip last part
                }
                if let Some(part) = upload.parts.get(part_num) {
                    if part.size < MIN_PART_SIZE as u64 {
                        return Err(AwsServiceError::aws_error(
                            StatusCode::BAD_REQUEST,
                            "EntityTooSmall",
                            "Your proposed upload is smaller than the minimum allowed object size.",
                        ));
                    }
                }
            }
        }

        // Assemble the object from its parts OFF-LOCK. Bodies are read via the
        // state-free `read_body_uncached`, so no S3 lock is held during the
        // (potentially large) concatenation, hashing, and disk persist below.
        //
        // The read+concat+hash of every part is synchronous, blocking disk IO
        // (`std::fs::read`) that can process multi-GB objects, so run it inside
        // `run_blocking_io`: on the multi-threaded runtime this hands the tokio
        // worker's other tasks to a sibling thread for the duration instead of
        // stalling them (and risking the client timeout) — the same helper
        // GetObject uses (bug-hunt 2026-07-19). Behavior/response are
        // unchanged; only where the CPU/IO runs differs.
        let checksum_algorithm = upload.checksum_algorithm.clone();
        let (combined_data, md5_digests, part_sizes, part_checksum_digests) =
            super::objects::run_blocking_io(|| {
                let mut combined_data = Vec::new();
                let mut md5_digests = Vec::new();
                let mut part_sizes = Vec::new();
                // Per-part raw additional-checksum digests, collected in
                // part-number order so we can build the COMPOSITE checksum
                // (hash-of-part-hashes) AWS returns for multipart objects.
                let mut part_checksum_digests: Vec<Vec<u8>> = Vec::new();

                for (part_num, submitted_etag) in &sorted_parts {
                    let part = upload.parts.get(part_num).ok_or_else(|| {
                        AwsServiceError::aws_error(
                            StatusCode::BAD_REQUEST,
                            "InvalidPart",
                            "One or more of the specified parts could not be found.",
                        )
                    })?;
                    if submitted_etag != &part.etag {
                        return Err(AwsServiceError::aws_error(
                            StatusCode::BAD_REQUEST,
                            "InvalidPart",
                            "One or more of the specified parts could not be found. The part may not have been uploaded, or the specified entity tag may not have matched the part's entity tag.",
                        ));
                    }
                    let part_bytes = crate::state::S3State::read_body_uncached(&part.body)
                        .map_err(super::io_to_aws)?;
                    let part_md5 = Md5::digest(&part_bytes);
                    // Fail closed if the bytes just read no longer hash to the
                    // part's recorded etag. We snapshot (clone) the upload —
                    // including each part's fixed disk path and etag — then read
                    // bodies off-lock. In disk mode a concurrent UploadPart of
                    // the same part number rewrites part-NNNNN.bin after the
                    // snapshot, so without this check Complete would assemble the
                    // *new* bytes while validating them against the *old* cloned
                    // etag, producing an object whose content doesn't match the
                    // parts the client committed (bug-hunt 2026-06-13, finding
                    // 4.1). Memory-mode parts are immutable, so this never trips
                    // for them.
                    if format!("{:x}", part_md5) != part.etag {
                        return Err(AwsServiceError::aws_error(
                            StatusCode::BAD_REQUEST,
                            "InvalidPart",
                            "One or more of the specified parts could not be found. The part may not have been uploaded, or the specified entity tag may not have matched the part's etag.",
                        ));
                    }
                    if let Some(algo) = checksum_algorithm.as_deref() {
                        part_checksum_digests.push(super::compute_checksum_raw(algo, &part_bytes));
                    }
                    combined_data.extend_from_slice(&part_bytes);
                    md5_digests.extend_from_slice(&part_md5);
                    part_sizes.push((*part_num, part_bytes.len() as u64));
                }
                Ok::<_, AwsServiceError>((
                    combined_data,
                    md5_digests,
                    part_sizes,
                    part_checksum_digests,
                ))
            })?;

        // Multipart ETag: MD5(concat(part_md5_digests))-N
        let combined_md5 = Md5::digest(&md5_digests);
        let etag = format!("{:x}-{}", combined_md5, sorted_parts.len());
        // Additional-checksum for a multipart object is COMPOSITE:
        // base64(HASH(concat(raw part digests)))-N - NOT a checksum over
        // the whole reassembled object.
        let checksum_value = upload
            .checksum_algorithm
            .as_deref()
            .and_then(|algo| super::compute_composite_checksum(algo, &part_checksum_digests));
        let data = Bytes::from(combined_data);
        let object_size = data.len() as u64;

        let tags = if let Some(ref tagging) = upload.tagging {
            parse_url_encoded_tags(tagging).into_iter().collect()
        } else {
            std::collections::BTreeMap::new()
        };

        let obj = S3Object {
            key: key.to_string(),
            size: data.len() as u64,
            body: crate::state::memory_body(data),
            content_type: upload.content_type.clone(),
            etag: etag.clone(),
            last_modified: Utc::now(),
            metadata: upload.metadata.clone(),
            storage_class: upload.storage_class.clone(),
            tags,
            acl_grants: upload.acl_grants.clone(),
            acl_owner_id: Some(acl_owner_id),
            parts_count: Some(sorted_parts.len() as u32),
            part_sizes: Some(part_sizes),
            sse_algorithm: upload.sse_algorithm.clone(),
            sse_kms_key_id: upload.sse_kms_key_id.clone(),
            // Assigned at commit from the bucket's versioning state.
            version_id: None,
            checksum_algorithm: upload.checksum_algorithm.clone(),
            checksum_value: checksum_value.clone(),
            cache_control: upload.cache_control.clone(),
            content_disposition: upload.content_disposition.clone(),
            content_language: upload.content_language.clone(),
            expires: upload.expires.clone(),
            ..Default::default()
        };

        // Commit under the write lock. The expensive work — reading,
        // assembling, and hashing every part — already ran off-lock above; only
        // the validation, the durable persist, and the in-memory insert happen
        // here. Crucially the persist runs *after* the upload + precondition
        // checks pass and within the same lock hold as the insert, so a rejected
        // completion never leaves a half-written object on disk while memory
        // says otherwise (Cubic, 4.7; the off-lock persist could diverge on a
        // concurrent complete). A concurrent Abort/Complete that removed the
        // upload during assembly is honored (idempotent re-completion) rather
        // than resurrected (bug-audit 2026-05-28, 4.3 class).
        let version_id = {
            let mut accts = self.state.write();
            let b = accts
                .get_or_create(account_id)
                .buckets
                .get_mut(bucket)
                .ok_or_else(|| no_such_bucket(bucket))?;
            if !b.multipart_uploads.contains_key(upload_id) {
                if let Some(existing) = b.objects.get(key) {
                    return Ok(completion_xml_response(
                        bucket,
                        &b.region,
                        key,
                        &existing.etag,
                        "",
                    ));
                }
                return Err(no_such_upload(upload_id));
            }
            // The object may have been created concurrently after the
            // phase-1 snapshot, so revalidate `If-None-Match: *` here rather
            // than trusting the stale `already_has_object`. Leave the upload
            // intact for retry/abort; nothing has been persisted yet.
            if if_none_match.as_deref() == Some("*") && b.objects.contains_key(key) {
                return Err(precondition_failed("If-None-Match"));
            }
            // Checks passed -- persist, then commit to memory, all under the
            // lock, recording the completed upload as a new version the way
            // every write does (a pre-versioning object is preserved as the
            // null version on an enabled bucket; a suspended bucket's
            // completion takes over the null version). In disk mode
            // `mpu_complete` streams the parts straight into the object file
            // and returns a disk-backed ref, so the object is never written a
            // second time from RAM (bug-hunt 2026-06-24, 4.1). In memory mode
            // it returns an empty placeholder, so the assembled in-memory body
            // is kept.
            // The lock requested at CreateMultipartUpload; the bucket's
            // default retention is applied at commit when none was.
            let lock = upload.lock.clone();
            let mut obj = obj;
            obj.lock_mode = lock.mode;
            obj.lock_retain_until = lock.retain_until;
            obj.lock_legal_hold = lock.legal_hold;
            let store = self.store.clone();
            let version_id = crate::service::objects::commit_object_version_with(
                b,
                bucket,
                Some(&*store),
                obj,
                |store, obj, meta| {
                    let assembled = store.mpu_complete(
                        bucket,
                        upload_id,
                        &obj.key,
                        meta.version_id.as_deref(),
                        meta,
                    )?;
                    // Memory mode returns only a placeholder; keep the
                    // assembled in-memory body.
                    Ok(match assembled {
                        BodyRef::Memory(_) => obj.body.clone(),
                        disk => disk,
                    })
                },
            )
            .map_err(super::persistence_error)?;
            b.multipart_uploads.remove(upload_id);
            version_id
        };

        let mut headers = HeaderMap::new();
        if let Some(vid) = &version_id {
            headers.insert("x-amz-version-id", vid.parse().unwrap());
        }
        if let Some(algo) = &upload.sse_algorithm {
            headers.insert("x-amz-server-side-encryption", algo.parse().unwrap());
        }
        if let Some(kid) = &upload.sse_kms_key_id {
            headers.insert(
                "x-amz-server-side-encryption-aws-kms-key-id",
                kid.parse().unwrap(),
            );
        }

        let location = xml_escape(&completion_location(bucket, &bucket_region, key));
        // Surface the per-algorithm checksum element AWS emits so
        // SDK clients that round-trip Complete -> Get can verify
        // integrity end-to-end.
        let checksum_xml = match (upload.checksum_algorithm.as_deref(), &checksum_value) {
            (Some(algo), Some(value)) => format!(
                "<Checksum{algo}>{val}</Checksum{algo}>",
                algo = algo,
                val = xml_escape(value),
            ),
            _ => String::new(),
        };
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <CompleteMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Location>{location}</Location>\
             <Bucket>{}</Bucket>\
             <Key>{}</Key>\
             <ETag>&quot;{}&quot;</ETag>\
             {checksum_xml}\
             </CompleteMultipartUploadResult>",
            xml_escape(bucket),
            xml_escape(key),
            xml_escape(&etag),
        );

        // The write lock was already released after committing the object, so
        // event delivery (which may re-enter the S3 service to read state)
        // cannot deadlock against an outstanding write.
        let bucket_name = bucket.to_string();
        let obj_key = key.to_string();
        let obj_size = object_size;
        let obj_etag = etag.clone();
        if let Some(ref config) = notification_config {
            super::deliver_notifications(
                &self.delivery,
                config,
                &super::notifications::ObjectEvent {
                    event_name: "ObjectCreated:CompleteMultipartUpload",
                    bucket_name: &bucket_name,
                    requester_account: &req.account_id,
                    key: &obj_key,
                    size: obj_size,
                    etag: &obj_etag,
                    region: &region,
                    version_id: version_id.as_deref(),
                },
                Some(&self.state),
            );
        }

        Ok(AwsResponse {
            status: StatusCode::OK,
            content_type: "application/xml".to_string(),
            body: body.into(),
            headers,
        })
    }

    pub(super) fn abort_multipart_upload(
        &self,
        account_id: &str,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let mut accts = self.state.write();
        let state = accts.get_or_create(account_id);
        // AbortMultipartUpload only declares NoSuchUpload per the Smithy
        // model; collapse missing-bucket into NoSuchUpload for strict
        // conformance (no bucket -> no upload to abort).
        let b = state
            .buckets
            .get_mut(bucket)
            .ok_or_else(|| no_such_upload(upload_id))?;

        // Validate upload exists and belongs to the requested key
        match b.multipart_uploads.get(upload_id) {
            Some(upload) if upload.key != key => {
                return Err(no_such_upload(upload_id));
            }
            None => {
                return Err(no_such_upload(upload_id));
            }
            _ => {}
        }
        // Store-first for consistency with other MPU ops.
        self.store
            .mpu_abort(bucket, upload_id)
            .map_err(super::persistence_error)?;
        b.multipart_uploads.remove(upload_id);

        Ok(AwsResponse {
            status: StatusCode::NO_CONTENT,
            content_type: "application/xml".to_string(),
            body: Bytes::new().into(),
            headers: HeaderMap::new(),
        })
    }

    pub(super) fn list_multipart_uploads(
        &self,
        account_id: &str,
        bucket: &str,
        query: &std::collections::HashMap<String, String>,
    ) -> Result<AwsResponse, AwsServiceError> {
        let accts = self.state.read();
        let __empty = crate::state::S3State::new(account_id, "us-east-1");
        let state = accts.get(account_id).unwrap_or(&__empty);
        let b = state
            .buckets
            .get(bucket)
            .ok_or_else(|| no_such_bucket(bucket))?;

        let prefix = query.get("prefix").cloned().unwrap_or_default();
        let delimiter = query.get("delimiter").cloned().unwrap_or_default();
        let key_marker = query.get("key-marker").cloned().unwrap_or_default();
        let upload_id_marker = query.get("upload-id-marker").cloned().unwrap_or_default();
        // AWS clamps max-uploads to 1..=1000, defaulting to 1000.
        let max_uploads = query
            .get("max-uploads")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1000)
            .clamp(1, 1000);

        // AWS orders ListMultipartUploads by key, then by upload-id.
        let mut sorted_uploads: Vec<_> = b
            .multipart_uploads
            .values()
            .filter(|u| u.key.starts_with(&prefix))
            .collect();
        sorted_uploads.sort_by(|a, c| a.key.cmp(&c.key).then(a.upload_id.cmp(&c.upload_id)));

        // Skip everything at or before (key-marker, upload-id-marker). With an
        // upload-id-marker the boundary key is resumed mid-way; without one,
        // every upload up to and including the marker key is skipped.
        // A key-marker that is itself a CommonPrefix (the NextKeyMarker of a
        // page that ended on a rolled-up prefix) resumes after every key
        // rolled into it, so the prefix is not listed twice.
        let marker_is_common_prefix = !delimiter.is_empty()
            && key_marker.starts_with(&prefix)
            && key_marker.len() > prefix.len()
            && key_marker.ends_with(&delimiter)
            && key_marker[prefix.len()..].find(&delimiter)
                == Some(key_marker.len() - prefix.len() - delimiter.len());
        let after_marker = |key: &str, upload_id: &str| -> bool {
            if key_marker.is_empty() {
                return true;
            }
            if marker_is_common_prefix && key.starts_with(key_marker.as_str()) {
                return false;
            }
            match key.cmp(key_marker.as_str()) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => {
                    !upload_id_marker.is_empty() && upload_id > upload_id_marker.as_str()
                }
            }
        };

        // Uploads and distinct CommonPrefixes together fill the page: each
        // rolled-up prefix counts once toward max-uploads, as on AWS. Keys
        // sharing a prefix are contiguous in key order, so a prefix is
        // complete once the next key falls outside it.
        let mut uploads_xml = String::new();
        let mut common_prefixes: Vec<String> = Vec::new();
        let mut emitted = 0usize;
        let mut is_truncated = false;
        let mut next_key_marker = String::new();
        let mut next_upload_id_marker = String::new();

        for upload in sorted_uploads
            .iter()
            .filter(|u| after_marker(&u.key, &u.upload_id))
        {
            let rolled_up = if delimiter.is_empty() {
                None
            } else {
                let rest = &upload.key[prefix.len()..];
                rest.find(&delimiter)
                    .map(|idx| format!("{}{}", prefix, &rest[..idx + delimiter.len()]))
            };
            if let Some(cp) = &rolled_up {
                if common_prefixes.last() == Some(cp) {
                    continue;
                }
            }
            if emitted >= max_uploads {
                // The page is full. The Next*Marker pair names the LAST entry
                // on the page so the next page resumes strictly after it.
                is_truncated = true;
                break;
            }
            emitted += 1;
            if let Some(cp) = rolled_up {
                next_key_marker = cp.clone();
                next_upload_id_marker.clear();
                common_prefixes.push(cp);
                continue;
            }
            uploads_xml.push_str(&format!(
                "<Upload>\
                 <Key>{}</Key>\
                 <UploadId>{}</UploadId>\
                 <Initiated>{}</Initiated>\
                 <StorageClass>{}</StorageClass>\
                 </Upload>",
                xml_escape(&upload.key),
                xml_escape(&upload.upload_id),
                upload.initiated.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                xml_escape(&upload.storage_class),
            ));
            next_key_marker = upload.key.clone();
            next_upload_id_marker = upload.upload_id.clone();
        }

        let mut markers_xml = String::new();
        if !key_marker.is_empty() {
            markers_xml.push_str(&format!(
                "<KeyMarker>{}</KeyMarker>",
                xml_escape(&key_marker)
            ));
        }
        if !upload_id_marker.is_empty() {
            markers_xml.push_str(&format!(
                "<UploadIdMarker>{}</UploadIdMarker>",
                xml_escape(&upload_id_marker)
            ));
        }
        if is_truncated {
            markers_xml.push_str(&format!(
                "<NextKeyMarker>{}</NextKeyMarker>",
                xml_escape(&next_key_marker)
            ));
            // A page ending on a CommonPrefix has no upload to resume after.
            if !next_upload_id_marker.is_empty() {
                markers_xml.push_str(&format!(
                    "<NextUploadIdMarker>{}</NextUploadIdMarker>",
                    xml_escape(&next_upload_id_marker)
                ));
            }
        }
        if !prefix.is_empty() {
            markers_xml.push_str(&format!("<Prefix>{}</Prefix>", xml_escape(&prefix)));
        }
        if !delimiter.is_empty() {
            markers_xml.push_str(&format!(
                "<Delimiter>{}</Delimiter>",
                xml_escape(&delimiter)
            ));
        }
        let common_prefixes_xml: String = common_prefixes
            .iter()
            .map(|cp| {
                format!(
                    "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                    xml_escape(cp)
                )
            })
            .collect();

        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListMultipartUploadsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Bucket>{}</Bucket>\
             {markers_xml}\
             <MaxUploads>{max_uploads}</MaxUploads>\
             <IsTruncated>{is_truncated}</IsTruncated>\
             {uploads_xml}\
             {common_prefixes_xml}\
             </ListMultipartUploadsResult>",
            xml_escape(bucket),
        );
        Ok(s3_xml(StatusCode::OK, body))
    }

    pub(super) fn list_parts(
        &self,
        account_id: &str,
        req: &AwsRequest,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let max_parts: i64 = match req.query_params.get("max-parts") {
            Some(v) => v.parse().map_err(|_| {
                AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidArgument",
                    "Provided max-parts not an integer or within integer range",
                )
            })?,
            None => 1000,
        };
        let part_number_marker: i64 = match req.query_params.get("part-number-marker") {
            Some(v) => v.parse().map_err(|_| {
                AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "InvalidArgument",
                    "Provided part-number-marker not an integer or within integer range",
                )
            })?,
            None => 0,
        };

        // Validate max-parts and part-number-marker
        if max_parts < 0 {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "Argument max-parts must be an integer between 0 and 2147483647",
            ));
        }
        if max_parts > 2147483647 {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "Provided max-parts not an integer or within integer range",
            ));
        }
        if part_number_marker < 0 {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "Argument part-number-marker must be an integer between 0 and 2147483647",
            ));
        }
        if part_number_marker > 2147483647 {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "Provided part-number-marker not an integer or within integer range",
            ));
        }

        let accts = self.state.read();
        let __empty = crate::state::S3State::new(account_id, "us-east-1");
        let state = accts.get(account_id).unwrap_or(&__empty);
        let b = state
            .buckets
            .get(bucket)
            .ok_or_else(|| no_such_bucket(bucket))?;
        let upload = b
            .multipart_uploads
            .get(upload_id)
            .ok_or_else(|| no_such_upload(upload_id))?;
        if upload.key != key {
            return Err(no_such_upload(upload_id));
        }

        // Filter parts after marker and apply limit
        let all_parts: Vec<_> = upload
            .parts
            .values()
            .filter(|p| p.part_number as i64 > part_number_marker)
            .collect();
        let max = max_parts as usize;
        let is_truncated = all_parts.len() > max;
        let display_parts: Vec<_> = all_parts.into_iter().take(max).collect();

        let mut parts_xml = String::new();
        let mut next_marker: i64 = 0;
        for part in &display_parts {
            next_marker = part.part_number as i64;
            parts_xml.push_str(&format!(
                "<Part>\
                 <PartNumber>{}</PartNumber>\
                 <ETag>&quot;{}&quot;</ETag>\
                 <Size>{}</Size>\
                 <LastModified>{}</LastModified>\
                 </Part>",
                part.part_number,
                xml_escape(&part.etag),
                part.size,
                part.last_modified.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            ));
        }

        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListPartsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Bucket>{}</Bucket>\
             <Key>{}</Key>\
             <UploadId>{}</UploadId>\
             <PartNumberMarker>{part_number_marker}</PartNumberMarker>\
             <NextPartNumberMarker>{next_marker}</NextPartNumberMarker>\
             <MaxParts>{max_parts}</MaxParts>\
             <IsTruncated>{is_truncated}</IsTruncated>\
             {parts_xml}\
             </ListPartsResult>",
            xml_escape(bucket),
            xml_escape(key),
            xml_escape(upload_id),
        );
        Ok(s3_xml(StatusCode::OK, body))
    }
}

#[cfg(test)]
mod tests {
    use super::completion_location;

    #[test]
    fn completion_location_uses_the_bucket_region_partition() {
        assert_eq!(
            completion_location("b", "eu-central-1", "k/obj"),
            "https://b.s3.eu-central-1.amazonaws.com/k/obj"
        );
        assert_eq!(
            completion_location("b", "cn-north-1", "k"),
            "https://b.s3.cn-north-1.amazonaws.com.cn/k"
        );
    }
}
