//! Shared input validation for Bedrock runtime operations
//! (InvokeModel, InvokeModelWithResponseStream, InvokeModelWithBidirectionalStream,
//! Converse, ConverseStream, CountTokens, StartAsyncInvoke, GetAsyncInvoke,
//! ListAsyncInvokes).
//!
//! These checks mirror the `@length`, `@range`, `@pattern` and enum constraints
//! declared in the upstream Smithy model so that out-of-band requests surface
//! the same `ValidationException` real AWS would return, instead of slipping
//! through to the canned handlers.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use fakecloud_core::service::{AwsRequest, AwsServiceError};
use http::StatusCode;
use regex::Regex;
use serde_json::Value;

/// `@length: min=1 max=2048` on `InvokeModelIdentifier` / `ConversationalModelId`
/// (used by InvokeModel, InvokeModelWithResponseStream,
/// InvokeModelWithBidirectionalStream, Converse, ConverseStream).
pub(crate) fn validate_invoke_model_id(model_id: &str) -> Result<(), AwsServiceError> {
    validate_length("modelId", model_id, 1, 2048)?;
    validate_model_id_pattern(model_id)
}

/// `@length: min=1 max=256` on `FoundationModelVersionIdentifier`
/// (CountTokens) and `AsyncInvokeIdentifier` (StartAsyncInvoke modelId).
pub(crate) fn validate_short_model_id(model_id: &str) -> Result<(), AwsServiceError> {
    validate_length("modelId", model_id, 1, 256)?;
    validate_model_id_pattern(model_id)
}

/// Reject obviously-bogus modelId values. The upstream Smithy regex is a
/// 600-character alternation — rather than mirror it byte-for-byte we
/// enforce the cheap structural checks that distinguish a real model
/// identifier from a stray URI template (`{modelId}`), an empty
/// percent-encoded path segment, or whitespace.
fn validate_model_id_pattern(model_id: &str) -> Result<(), AwsServiceError> {
    if model_id.contains('{') || model_id.contains('}') {
        return Err(validation(format!(
            "Value '{model_id}' at 'modelId' failed to satisfy constraint: Member must match a valid model identifier pattern"
        )));
    }
    if model_id.chars().any(|c| c.is_whitespace()) {
        return Err(validation(format!(
            "Value '{model_id}' at 'modelId' failed to satisfy constraint: whitespace is not allowed"
        )));
    }
    Ok(())
}

/// Validate a string length against `min..=max`. Pass `min=0` to skip the
/// lower bound. Empty values trip the min check when `min >= 1`.
pub(crate) fn validate_length(
    field: &str,
    value: &str,
    min: usize,
    max: usize,
) -> Result<(), AwsServiceError> {
    let len = value.chars().count();
    if len < min {
        return Err(validation(format!(
            "'{value}' at '{field}' failed to satisfy constraint: Member must have length greater than or equal to {min}"
        )));
    }
    if len > max {
        return Err(validation(format!(
            "Value at '{field}' failed to satisfy constraint: Member must have length less than or equal to {max}"
        )));
    }
    Ok(())
}

/// `@range: min=1 max=1000` on `MaxResults` (ListAsyncInvokes).
pub(crate) fn validate_range_i64(
    field: &str,
    value: i64,
    min: i64,
    max: i64,
) -> Result<(), AwsServiceError> {
    if value < min {
        return Err(validation(format!(
            "Value '{value}' at '{field}' failed to satisfy constraint: Member must have value greater than or equal to {min}"
        )));
    }
    if value > max {
        return Err(validation(format!(
            "Value '{value}' at '{field}' failed to satisfy constraint: Member must have value less than or equal to {max}"
        )));
    }
    Ok(())
}

/// Validate an enum value against the allowed set.
pub(crate) fn validate_enum(
    field: &str,
    value: &str,
    allowed: &[&str],
) -> Result<(), AwsServiceError> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(validation(format!(
        "Value '{value}' at '{field}' failed to satisfy constraint: Member must satisfy enum value set: [{}]",
        allowed.join(", ")
    )))
}

/// Allowed values for `@httpHeader X-Amzn-Bedrock-PerformanceConfig-Latency`.
const PERFORMANCE_CONFIG_LATENCY: &[&str] = &["standard", "optimized"];

/// Allowed values for `@httpHeader X-Amzn-Bedrock-Service-Tier`.
const SERVICE_TIER: &[&str] = &["priority", "default", "flex", "reserved"];

/// Allowed values for `@httpHeader X-Amzn-Bedrock-Trace`.
const TRACE: &[&str] = &["ENABLED", "DISABLED", "ENABLED_FULL"];

/// Validate shared invoke/converse headers (performanceConfigLatency,
/// serviceTier, trace) and the body-size cap on Body (max 25_000_000 bytes).
/// Returns the validation error on first violation.
pub(crate) fn validate_runtime_headers(req: &AwsRequest) -> Result<(), AwsServiceError> {
    if let Some(v) = req
        .headers
        .get("x-amzn-bedrock-performanceconfig-latency")
        .and_then(|h| h.to_str().ok())
    {
        validate_enum("performanceConfigLatency", v, PERFORMANCE_CONFIG_LATENCY)?;
    }
    if let Some(v) = req
        .headers
        .get("x-amzn-bedrock-service-tier")
        .and_then(|h| h.to_str().ok())
    {
        validate_enum("serviceTier", v, SERVICE_TIER)?;
    }
    if let Some(v) = req
        .headers
        .get("x-amzn-bedrock-trace")
        .and_then(|h| h.to_str().ok())
    {
        validate_enum("trace", v, TRACE)?;
    }
    // GuardrailIdentifier: @length max=2048 (no min). Header form.
    if let Some(v) = req
        .headers
        .get("x-amzn-bedrock-guardrailidentifier")
        .and_then(|h| h.to_str().ok())
    {
        validate_length("guardrailIdentifier", v, 0, 2048)?;
    }
    Ok(())
}

/// Validate the body-size cap on InvokeModel-style payloads (`@length max=25_000_000`).
pub(crate) fn validate_invoke_body_size(body: &[u8]) -> Result<(), AwsServiceError> {
    if body.len() > 25_000_000 {
        return Err(validation(
            "Value at 'body' failed to satisfy constraint: Member must have length less than or equal to 25000000",
        ));
    }
    Ok(())
}

/// Build a `ValidationException` with 400 status — the response shape declared
/// by every runtime operation we care about here.
pub(crate) fn validation<S: Into<String>>(message: S) -> AwsServiceError {
    AwsServiceError::aws_error(StatusCode::BAD_REQUEST, "ValidationException", message)
}

/// Validated `requestMetadata` key-value pairs, recorded on the invocation.
pub(crate) type RequestMetadata = BTreeMap<String, String>;

/// Header carrying InvokeModel's `requestMetadata` (`RequestMetadataJson`).
pub(crate) const REQUEST_METADATA_HEADER: &str = "x-amzn-bedrock-request-metadata";

/// `@pattern` shared by the `RequestMetadata` map's key and value members.
/// Inside the class, `,-.` is the range `,`..`.` (`,`, `-`, `.`), exactly as
/// the Smithy model spells it.
fn request_metadata_entry_regex(min: u32) -> &'static Regex {
    static KEY: OnceLock<Regex> = OnceLock::new();
    static VALUE: OnceLock<Regex> = OnceLock::new();
    let (cell, pat) = if min == 1 {
        (&KEY, r"^[a-zA-Z0-9\s:_@$#=/+,-.]{1,256}$")
    } else {
        (&VALUE, r"^[a-zA-Z0-9\s:_@$#=/+,-.]{0,256}$")
    };
    cell.get_or_init(|| Regex::new(pat).expect("static requestMetadata pattern"))
}

/// Validate a `RequestMetadata` map (`@length(1, 16)`; keys `@length(1, 256)`,
/// values `@length(0, 256)`, both constrained by the model's `@pattern`) and
/// return its pairs. The values are `@sensitive`, so messages never echo them.
pub(crate) fn validate_request_metadata_map(
    field: &str,
    value: &Value,
) -> Result<RequestMetadata, AwsServiceError> {
    let Value::Object(map) = value else {
        return Err(validation(format!(
            "Value at '{field}' failed to satisfy constraint: Member must be a map of string keys to string values"
        )));
    };
    if map.is_empty() {
        return Err(validation(format!(
            "Value at '{field}' failed to satisfy constraint: Member must have length greater than or equal to 1"
        )));
    }
    if map.len() > 16 {
        return Err(validation(format!(
            "Value at '{field}' failed to satisfy constraint: Member must have length less than or equal to 16"
        )));
    }
    let mut out = RequestMetadata::new();
    for (key, v) in map {
        let key_len = key.chars().count();
        if key_len == 0 || key_len > 256 {
            return Err(validation(format!(
                "Value at '{field}' failed to satisfy constraint: Map keys must have length between 1 and 256"
            )));
        }
        if !request_metadata_entry_regex(1).is_match(key) {
            return Err(validation(format!(
                "Value at '{field}' failed to satisfy constraint: Map keys must satisfy regular expression pattern: ^[a-zA-Z0-9\\s:_@$#=/+,-.]{{1,256}}$"
            )));
        }
        let Value::String(v) = v else {
            return Err(validation(format!(
                "Value at '{field}.{key}' failed to satisfy constraint: Member must be a string"
            )));
        };
        if v.chars().count() > 256 {
            return Err(validation(format!(
                "Value at '{field}.{key}' failed to satisfy constraint: Member must have length less than or equal to 256"
            )));
        }
        if !request_metadata_entry_regex(0).is_match(v) {
            return Err(validation(format!(
                "Value at '{field}.{key}' failed to satisfy constraint: Member must satisfy regular expression pattern: ^[a-zA-Z0-9\\s:_@$#=/+,-.]{{0,256}}$"
            )));
        }
        out.insert(key.clone(), v.clone());
    }
    Ok(out)
}

/// Validate and parse InvokeModel's `X-Amzn-Bedrock-Request-Metadata` header:
/// `RequestMetadataJson` `@length(max: 8500)`, carrying a JSON object of the
/// same key-value pairs the Converse `requestMetadata` map holds. The length
/// is measured on the raw header bytes (lossily decoded), so a header with
/// non-ASCII bytes is still bounded rather than skipped.
pub(crate) fn request_metadata_header(
    req: &AwsRequest,
) -> Result<Option<RequestMetadata>, AwsServiceError> {
    let Some(raw) = req.headers.get(REQUEST_METADATA_HEADER) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(raw.as_bytes());
    validate_length("requestMetadata", &text, 0, 8500)?;
    let parsed: Value = serde_json::from_str(&text).map_err(|_| {
        validation(
            "Value at 'requestMetadata' failed to satisfy constraint: Member must be a JSON object of string keys to string values",
        )
    })?;
    validate_request_metadata_map("requestMetadata", &parsed).map(Some)
}

/// Validate Converse / ConverseStream's optional `requestMetadata` body map.
pub(crate) fn request_metadata_body(
    body: &Value,
) -> Result<Option<RequestMetadata>, AwsServiceError> {
    match body.get("requestMetadata") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => validate_request_metadata_map("requestMetadata", v).map(Some),
    }
}
