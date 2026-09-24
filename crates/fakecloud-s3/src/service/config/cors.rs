//! `S3Service` `cors` family — extracted from service.rs by audit-2026-05-19.

use super::*;
use crate::service::parse_cors_config;

impl S3Service {
    // ---- CORS ----

    pub(crate) fn put_bucket_cors(
        &self,
        account_id: &str,
        req: &AwsRequest,
        bucket: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body_str = std::str::from_utf8(&req.body).unwrap_or("").to_string();

        // Validate CORS configuration
        let rule_count = body_str.matches("<CORSRule>").count();
        if rule_count == 0 || rule_count > 100 {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "MalformedXML",
                "The XML you provided was not well-formed or did not validate against our published schema",
            ));
        }

        // `parse_cors_config` stops at the first `<CORSRule>` it cannot close,
        // returning only the rules it managed to parse. Requiring the counts to
        // agree turns an unterminated rule into `MalformedXML` instead of a
        // stored config whose tail was silently dropped.
        let parsed = parse_cors_config(&body_str);
        if parsed.len() != rule_count {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "MalformedXML",
                "The XML you provided was not well-formed or did not validate against our published schema",
            ));
        }

        // Validate HTTP methods
        let valid_methods = ["GET", "PUT", "POST", "DELETE", "HEAD"];
        let mut remaining = body_str.as_str();
        while let Some(start) = remaining.find("<AllowedMethod>") {
            let after = &remaining[start + 15..];
            if let Some(end) = after.find("</AllowedMethod>") {
                let method = after[..end].trim();
                if !valid_methods.contains(&method) {
                    return Err(AwsServiceError::aws_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidRequest",
                        format!(
                            "Found unsupported HTTP method in CORS config. Unsupported method is {method}"
                        ),
                    ));
                }
                remaining = &after[end + 16..];
            } else {
                // Opening tag without a matching closer is malformed
                // XML; reject instead of saving a half-parsed config.
                return Err(AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "MalformedXML",
                    "The XML you provided was not well-formed or did not validate against our published schema",
                ));
            }
        }

        // `AllowedMethods` and `AllowedOrigins` are both required members of
        // `CORSRule`. A rule missing either matches nothing at request time, so
        // accepting one would leave the bucket silently CORS-dead rather than
        // telling the caller their config is wrong — AWS rejects it outright.
        // An empty or whitespace-only value counts as missing: `<AllowedOrigin></AllowedOrigin>`
        // parses to `""`, which matches no real request, so accepting it stores
        // the same CORS-dead rule as omitting the tag entirely. Runs after the
        // method validation above so an empty `<AllowedMethod>` still gets the
        // AWS-shaped error naming the offending value.
        for rule in parsed {
            // A rule needs at least one usable value, not every value usable: a
            // stray blank tag alongside a real origin still leaves the rule
            // live, and rejecting that would refuse a config real S3 accepts.
            let usable = |vs: &[String]| vs.iter().any(|v| !v.is_empty());
            if !usable(&rule.allowed_methods) || !usable(&rule.allowed_origins) {
                return Err(AwsServiceError::aws_error(
                    StatusCode::BAD_REQUEST,
                    "MalformedXML",
                    "The XML you provided was not well-formed or did not validate against our published schema",
                ));
            }

            // AWS allows at most one `*` per AllowedOrigin / AllowedHeader.
            // Both are deny gates at request time, and the matchers read only
            // the first `*`, so a second one would be treated as a literal and
            // quietly make the rule match nothing. Reject at write time, where
            // the caller can still see which value is wrong.
            for (label, values) in [
                ("AllowedOrigin", &rule.allowed_origins),
                ("AllowedHeader", &rule.allowed_headers),
            ] {
                if let Some(bad) = values.iter().find(|v| v.matches('*').count() > 1) {
                    return Err(AwsServiceError::aws_error(
                        StatusCode::BAD_REQUEST,
                        "InvalidRequest",
                        format!("{label} \"{bad}\" can not have more than one wildcard."),
                    ));
                }
            }
        }

        let mut accts = self.state.write();
        let state = accts.get_or_create(account_id);
        let b = state
            .buckets
            .get_mut(bucket)
            .ok_or_else(|| no_such_bucket(bucket))?;
        b.cors_config = Some(body_str.clone());
        self.store
            .put_bucket_subresource(bucket, BucketSubresource::Cors, &body_str)
            .map_err(crate::service::persistence_error)?;
        Ok(empty_response(StatusCode::OK))
    }

    pub(crate) fn get_bucket_cors(
        &self,
        account_id: &str,
        bucket: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let accts = self.state.read();
        let __empty = crate::state::S3State::new(account_id, "us-east-1");
        let state = accts.get(account_id).unwrap_or(&__empty);
        let b = state
            .buckets
            .get(bucket)
            .ok_or_else(|| no_such_bucket(bucket))?;
        match &b.cors_config {
            Some(config) => Ok(s3_xml(StatusCode::OK, config.clone())),
            None => Err(AwsServiceError::aws_error_with_fields(
                StatusCode::NOT_FOUND,
                "NoSuchCORSConfiguration",
                "The CORS configuration does not exist",
                vec![("BucketName".to_string(), bucket.to_string())],
            )),
        }
    }

    pub(crate) fn delete_bucket_cors(
        &self,
        account_id: &str,
        bucket: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let mut accts = self.state.write();
        let state = accts.get_or_create(account_id);
        let b = state
            .buckets
            .get_mut(bucket)
            .ok_or_else(|| no_such_bucket(bucket))?;
        b.cors_config = None;
        self.store
            .delete_bucket_subresource(bucket, BucketSubresource::Cors)
            .map_err(crate::service::persistence_error)?;
        Ok(empty_response(StatusCode::NO_CONTENT))
    }
}
