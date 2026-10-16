//! Model-derived input validation for Amazon SageMaker operations.
//!
//! Every SageMaker input member is carried in the awsJson1.1 request body, so
//! the rules generated from the Smithy model (see `src/generated.rs`) record,
//! per top-level input member, whether it is `@required` and its `@length` /
//! `@range` / `@enum` constraints. This validator enforces exactly those,
//! rejecting a violating request with SageMaker's `ValidationException`.
//! `@pattern` is intentionally not enforced (it is not part of the constraint
//! set exercised here and enforcing it would reject otherwise-valid inputs).

use http::StatusCode;
use serde_json::{Map, Value};

use fakecloud_core::service::AwsServiceError;

use crate::generated::{OpMeta, K};

/// SageMaker's canonical input-validation error. The model does not enumerate a
/// `ValidationException` shape on most operations, but the live API returns it
/// for malformed input, and it is registered as a service-wide common error in
/// the conformance probe.
pub const VALIDATION_ERROR: &str = "ValidationException";

pub fn invalid(msg: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(StatusCode::BAD_REQUEST, VALIDATION_ERROR, msg)
}

/// A workforce's VPC configuration is only usable (and is only ever reported
/// back, as `WorkforceVpcConfigResponse`, where all three are required) with a
/// VPC, at least one security group and at least one subnet. The request shape
/// leaves each optional, so an incomplete configuration is rejected here.
fn validate_workforce_vpc_config(
    meta: &OpMeta,
    body: &Map<String, Value>,
) -> Result<(), AwsServiceError> {
    if !matches!(meta.op, "CreateWorkforce" | "UpdateWorkforce") {
        return Ok(());
    }
    let Some(cfg) = body.get("WorkforceVpcConfig").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let non_empty_list = |k: &str| {
        cfg.get(k)
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
    };
    let has_vpc = cfg
        .get("VpcId")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if !has_vpc || !non_empty_list("SecurityGroupIds") || !non_empty_list("Subnets") {
        return Err(invalid(
            "WorkforceVpcConfig must specify VpcId, at least one SecurityGroupIds entry \
             and at least one Subnets entry.",
        ));
    }
    Ok(())
}

/// Validate a request body against the operation's model-derived constraints.
pub fn validate(meta: &OpMeta, body: &Map<String, Value>) -> Result<(), AwsServiceError> {
    validate_workforce_vpc_config(meta, body)?;
    for rule in meta.rules {
        let present = body.get(rule.wire).map(|v| !v.is_null()).unwrap_or(false);
        if rule.req && !present {
            return Err(invalid(format!(
                "Missing required parameter: {}",
                rule.wire
            )));
        }
        if !present {
            continue;
        }
        let val = &body[rule.wire];

        // String-shaped members: length + enum.
        if matches!(rule.kind, K::Str) {
            let s = match val {
                Value::String(s) => s,
                _ => {
                    return Err(invalid(format!("Member {} must be a string.", rule.wire)));
                }
            };
            let len = s.chars().count() as u64;
            if let Some(min) = rule.min_len {
                if len < min {
                    return Err(invalid(format!(
                        "Member {} is shorter than the minimum length {min}.",
                        rule.wire
                    )));
                }
            }
            if let Some(max) = rule.max_len {
                if len > max {
                    return Err(invalid(format!(
                        "Member {} is longer than the maximum length {max}.",
                        rule.wire
                    )));
                }
            }
            if !rule.enums.is_empty() && !rule.enums.contains(&s.as_str()) {
                return Err(invalid(format!(
                    "Member {} has an invalid enum value.",
                    rule.wire
                )));
            }
        }

        // Numeric members: range.
        if matches!(rule.kind, K::Int) && (rule.min_val.is_some() || rule.max_val.is_some()) {
            if let Some(n) = val.as_i64() {
                if let Some(min) = rule.min_val {
                    if n < min {
                        return Err(invalid(format!(
                            "Member {} is below the minimum {min}.",
                            rule.wire
                        )));
                    }
                }
                if let Some(max) = rule.max_val {
                    if n > max {
                        return Err(invalid(format!(
                            "Member {} is above the maximum {max}.",
                            rule.wire
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::OPS;

    fn meta(op: &str) -> &'static OpMeta {
        OPS.iter().find(|m| m.op == op).unwrap()
    }

    #[test]
    fn workforce_vpc_config_must_be_complete() {
        let m = meta("CreateWorkforce");
        let body = |cfg: Value| {
            let mut b = Map::new();
            b.insert("WorkforceName".to_string(), Value::String("wf".to_string()));
            b.insert("WorkforceVpcConfig".to_string(), cfg);
            b
        };
        assert!(validate(m, &body(serde_json::json!({"VpcId": "vpc-1"}))).is_err());
        assert!(validate(
            m,
            &body(serde_json::json!({
                "VpcId": "vpc-1",
                "SecurityGroupIds": [],
                "Subnets": ["subnet-1"]
            }))
        )
        .is_err());
        assert!(validate(
            m,
            &body(serde_json::json!({
                "VpcId": "vpc-1",
                "SecurityGroupIds": ["sg-1"],
                "Subnets": ["subnet-1"]
            }))
        )
        .is_ok());
        // Without a VPC config the workforce is valid.
        let mut plain = Map::new();
        plain.insert("WorkforceName".to_string(), Value::String("wf".to_string()));
        assert!(validate(m, &plain).is_ok());
    }

    #[test]
    fn rejects_missing_required_member() {
        // CreateModel requires ModelName.
        let m = meta("CreateModel");
        let body = Map::new();
        assert!(validate(m, &body).is_err());
    }

    #[test]
    fn accepts_valid_body() {
        let m = meta("CreateModel");
        let mut body = Map::new();
        body.insert("ModelName".to_string(), Value::String("m".to_string()));
        assert!(validate(m, &body).is_ok());
    }

    #[test]
    fn rejects_invalid_enum() {
        // CreateApp has an AppType enum.
        let m = meta("CreateApp");
        let mut body = Map::new();
        body.insert("DomainId".to_string(), Value::String("d".to_string()));
        body.insert(
            "AppType".to_string(),
            Value::String("BogusType".to_string()),
        );
        body.insert("AppName".to_string(), Value::String("a".to_string()));
        assert!(validate(m, &body).is_err());
    }
}
