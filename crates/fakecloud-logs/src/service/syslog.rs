use http::StatusCode;
use serde_json::json;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use super::{extract_log_group_from_arn, require_str, LogsService};
use crate::state::SyslogConfiguration;
use crate::validation::{
    validate_optional_range_i64, validate_optional_string_length, validate_string_length,
};
use chrono::Utc;

/// Resolve a `logGroupIdentifier` (a log group name or ARN) to the bare log
/// group name used as the state key.
fn identifier_to_group_name(identifier: &str) -> Result<String, AwsServiceError> {
    if identifier.starts_with("arn:") {
        extract_log_group_from_arn(identifier).ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "InvalidParameterException",
                format!("Invalid ARN: {identifier}"),
            )
        })
    } else {
        Ok(identifier.to_string())
    }
}

/// Validate a VPC endpoint id against the Smithy pattern `^vpce-[0-9a-f]{1,64}$`.
fn validate_vpc_endpoint_id(value: &str) -> Result<(), AwsServiceError> {
    let rest = value.strip_prefix("vpce-").filter(|r| {
        !r.is_empty()
            && r.len() <= 64
            && r.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    });
    if rest.is_some() {
        Ok(())
    } else {
        Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "InvalidParameterException",
            format!("vpcEndpointId does not match required pattern: {value}"),
        ))
    }
}

/// Validate a `logGroupIdentifier` (name or ARN) against its model
/// constraints: length 1..=2048 and pattern `^[\w#+=/:,.@-]*$`.
fn validate_log_group_identifier(value: &str) -> Result<(), AwsServiceError> {
    validate_string_length("logGroupIdentifier", value, 1, 2048)?;
    let ok = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || "#+=/:,.@-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "InvalidParameterException",
            format!("logGroupIdentifier does not match required pattern: {value}"),
        ))
    }
}

impl LogsService {
    // ---- Syslog configurations ----

    pub(crate) fn put_syslog_configuration(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let identifier = require_str(&body, "logGroupIdentifier")?.to_string();
        let group_name = identifier_to_group_name(&identifier)?;

        let vpc_endpoint_id = body["vpcEndpointId"].as_str().map(|s| s.to_string());
        if let Some(id) = &vpc_endpoint_id {
            validate_vpc_endpoint_id(id)?;
        }

        let mut accounts = self.state.write();
        let state = accounts.regional_mut(&req.account_id, &req.region);
        let group = state.log_groups.get(&group_name).ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "ResourceNotFoundException",
                format!("The specified log group does not exist: {group_name}"),
            )
        })?;
        let log_group_arn = group.log_group_arn().to_string();

        let created_at = state
            .syslog_configurations
            .get(&group_name)
            .map(|c| c.created_at)
            .unwrap_or_else(|| Utc::now().timestamp_millis());

        state.syslog_configurations.insert(
            group_name,
            SyslogConfiguration {
                log_group_arn,
                source_type: "VPCE".to_string(),
                vpc_endpoint_id,
                created_at,
            },
        );

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }

    pub(crate) fn list_syslog_configurations(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        if let Some(identifier) = body["logGroupIdentifier"].as_str() {
            validate_log_group_identifier(identifier)?;
        }
        let group_filter = body["logGroupIdentifier"]
            .as_str()
            .map(identifier_to_group_name)
            .transpose()?;
        let vpc_filter = body["vpcEndpointId"].as_str();
        if let Some(id) = vpc_filter {
            validate_vpc_endpoint_id(id)?;
        }
        validate_optional_range_i64("maxResults", body["maxResults"].as_i64(), 0, 50)?;
        validate_optional_string_length("nextToken", body["nextToken"].as_str(), 1, usize::MAX)?;

        let accounts = self.state.read();
        let empty = crate::state::LogsState::new(&req.account_id, &req.region);
        let state = accounts
            .regional(&req.account_id, &req.region)
            .unwrap_or(&empty);

        let configs: Vec<_> = state
            .syslog_configurations
            .iter()
            .filter(|(group_name, _)| {
                group_filter
                    .as_ref()
                    .is_none_or(|g| g == group_name.as_str())
            })
            .filter(|(_, c)| vpc_filter.is_none_or(|v| c.vpc_endpoint_id.as_deref() == Some(v)))
            .map(|(_, c)| {
                json!({
                    "logGroupArn": c.log_group_arn,
                    "sourceType": c.source_type,
                    "vpcEndpointId": c.vpc_endpoint_id,
                    "createdAt": c.created_at,
                })
            })
            .collect();
        // `maxResults` 0 (the model default) means "service default page".
        let (page, next) = super::paginate_offset(
            &configs,
            body["maxResults"].as_i64(),
            50,
            body["nextToken"].as_str(),
        );
        let mut out = json!({ "syslogConfigurations": page });
        if let Some(token) = next {
            out["nextToken"] = json!(token);
        }

        Ok(AwsResponse::json(
            StatusCode::OK,
            serde_json::to_string(&out).unwrap(),
        ))
    }

    pub(crate) fn delete_syslog_configuration(
        &self,
        req: &AwsRequest,
    ) -> Result<AwsResponse, AwsServiceError> {
        let body = req.json_body();
        let identifier = require_str(&body, "logGroupIdentifier")?.to_string();
        let group_name = identifier_to_group_name(&identifier)?;

        let mut accounts = self.state.write();
        let state = accounts.regional_mut(&req.account_id, &req.region);
        if state.syslog_configurations.remove(&group_name).is_none() {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "ResourceNotFoundException",
                format!("No syslog configuration found for log group: {group_name}"),
            ));
        }

        Ok(AwsResponse::json(StatusCode::OK, "{}"))
    }
}

#[cfg(test)]
mod tests {
    use crate::service::test_helpers::*;
    use serde_json::{json, Value};

    fn put(svc: &crate::service::LogsService, group: &str) {
        let req = make_request(
            "PutSyslogConfiguration",
            json!({ "logGroupIdentifier": group }),
        );
        svc.put_syslog_configuration(&req).unwrap();
    }

    fn list(svc: &crate::service::LogsService, body: Value) -> Result<Value, String> {
        let req = make_request("ListSyslogConfigurations", body);
        svc.list_syslog_configurations(&req)
            .map(|r| serde_json::from_slice(r.body.expect_bytes()).unwrap())
            .map_err(|e| e.code().to_string())
    }

    #[test]
    fn list_syslog_configurations_validates_and_paginates() {
        let svc = make_service();
        for g in ["sys-a", "sys-b", "sys-c"] {
            create_group(&svc, g);
            put(&svc, g);
        }

        let page1 = list(&svc, json!({ "maxResults": 2 })).unwrap();
        assert_eq!(page1["syslogConfigurations"].as_array().unwrap().len(), 2);
        let token = page1["nextToken"].as_str().unwrap().to_string();
        let page2 = list(&svc, json!({ "maxResults": 2, "nextToken": token })).unwrap();
        assert_eq!(page2["syslogConfigurations"].as_array().unwrap().len(), 1);
        assert!(page2.get("nextToken").is_none());
        // maxResults 0 (the model default) returns everything that fits a page.
        let all = list(&svc, json!({ "maxResults": 0 })).unwrap();
        assert_eq!(all["syslogConfigurations"].as_array().unwrap().len(), 3);

        for bad in [
            json!({ "maxResults": 51 }),
            json!({ "maxResults": -1 }),
            json!({ "nextToken": "" }),
            json!({ "logGroupIdentifier": "" }),
            json!({ "logGroupIdentifier": "a".repeat(2049) }),
            json!({ "logGroupIdentifier": "bad group!" }),
            json!({ "vpcEndpointId": "nope" }),
        ] {
            assert_eq!(
                list(&svc, bad.clone()).unwrap_err(),
                "InvalidParameterException",
                "{bad}"
            );
        }
    }
}
