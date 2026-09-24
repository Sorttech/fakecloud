//! `S3Service` `cors` family — extracted from service.rs by audit-2026-05-19.

use super::*;
use crate::service::parse_cors_config;

/// Remove `<!-- ... -->` spans so tag scanning sees only live markup.
/// An unterminated comment swallows the rest of the body, which then fails the
/// rule-count check as the malformed XML it is.
fn strip_xml_comments(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

const MALFORMED_XML: &str =
    "The XML you provided was not well-formed or did not validate against our published schema";

/// Validate a CORS configuration body, returning the AWS `(code, message)` for
/// the first problem found.
///
/// Shared so every writer of `cors_config` enforces the same rules: a config
/// that reaches storage without passing here can be one that matches no request
/// at all, leaving the bucket silently CORS-dead.
pub(crate) fn validate_cors_xml(body_str: &str) -> Result<(), (&'static str, String)> {
    // Count and parse over the same comment-free text. A `<CORSRule>` inside
    // an `<!-- ... -->` is not a rule, and counting it while the parser skips
    // it would reject a config real S3 accepts.
    let scannable = strip_xml_comments(body_str);

    // Validate CORS configuration
    let rule_count = scannable.matches("<CORSRule>").count();
    if rule_count == 0 || rule_count > 100 {
        return Err(("MalformedXML", MALFORMED_XML.to_string()));
    }

    // `parse_cors_config` stops at the first `<CORSRule>` it cannot close,
    // returning only the rules it managed to parse. Requiring the counts to
    // agree turns an unterminated rule into `MalformedXML` instead of a
    // stored config whose tail was silently dropped.
    let parsed = parse_cors_config(&scannable);
    if parsed.len() != rule_count {
        return Err(("MalformedXML", MALFORMED_XML.to_string()));
    }

    // Validate HTTP methods
    let valid_methods = ["GET", "PUT", "POST", "DELETE", "HEAD"];
    let mut remaining = body_str;
    while let Some(start) = remaining.find("<AllowedMethod>") {
        let after = &remaining[start + 15..];
        if let Some(end) = after.find("</AllowedMethod>") {
            let method = after[..end].trim();
            if !valid_methods.contains(&method) {
                return Err(("InvalidRequest", format!("Found unsupported HTTP method in CORS config. Unsupported method is {method}")));
            }
            remaining = &after[end + 16..];
        } else {
            // Opening tag without a matching closer is malformed
            // XML; reject instead of saving a half-parsed config.
            return Err(("MalformedXML", MALFORMED_XML.to_string()));
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
            return Err(("MalformedXML", MALFORMED_XML.to_string()));
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
                return Err((
                    "InvalidRequest",
                    format!("{label} \"{bad}\" can not have more than one wildcard."),
                ));
            }
        }

        // S3 supports no wildcard at all in ExposeHeader. Storing one would
        // work locally — Fetch reads a literal `*` as "expose all" for an
        // uncredentialed request — and then be rejected by real AWS.
        if let Some(bad) = rule.expose_headers.iter().find(|v| v.contains('*')) {
            return Err(("InvalidRequest", format!("ExposeHeader \"{bad}\" contains wildcard. We currently do not support wildcard for ExposeHeader.")));
        }
    }

    // A non-numeric `MaxAgeSeconds` parses to `None`, so the config would
    // round-trip looking healthy while every preflight silently shipped
    // without `Access-Control-Max-Age` and browsers re-preflighted each
    // request. Reject it instead, as S3 does.
    let mut remaining = scannable.as_str();
    while let Some(start) = remaining.find("<MaxAgeSeconds>") {
        let after = &remaining[start + 15..];
        let Some(end) = after.find("</MaxAgeSeconds>") else {
            return Err(("MalformedXML", MALFORMED_XML.to_string()));
        };
        if after[..end].trim().parse::<u32>().is_err() {
            return Err(("MalformedXML", MALFORMED_XML.to_string()));
        }
        remaining = &after[end + 16..];
    }

    Ok(())
}

impl S3Service {
    // ---- CORS ----

    pub(crate) fn put_bucket_cors(
        &self,
        account_id: &str,
        req: &AwsRequest,
        bucket: &str,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body_str = std::str::from_utf8(&req.body).unwrap_or("").to_string();

        validate_cors_xml(&body_str).map_err(|(code, message)| {
            AwsServiceError::aws_error(StatusCode::BAD_REQUEST, code, message)
        })?;

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
