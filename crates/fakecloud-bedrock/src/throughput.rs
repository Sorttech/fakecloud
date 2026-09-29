use chrono::Utc;
use http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::{ProvisionedThroughput, SharedBedrockState};

pub(crate) fn create_provisioned_model_throughput(
    state: &SharedBedrockState,
    req: &AwsRequest,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let provisioned_model_name = body["provisionedModelName"].as_str().ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "provisionedModelName is required",
        )
    })?;

    let model_id = body["modelId"].as_str().unwrap_or_default();
    let model_units = body["modelUnits"].as_i64().unwrap_or(1) as i32;

    if model_units < 1 {
        return Err(AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "modelUnits must be at least 1",
        ));
    }

    if let Some(duration) = body["commitmentDuration"].as_str() {
        if !["OneMonth", "SixMonths"].contains(&duration) {
            return Err(AwsServiceError::aws_error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                format!(
                    "Invalid commitmentDuration: {duration}. Valid values: OneMonth, SixMonths"
                ),
            ));
        }
    }

    let provisioned_model_id = Uuid::new_v4().to_string()[..12].to_string();
    let provisioned_model_arn =
        crate::arns::provisioned_model_arn(&req.region, &req.account_id, &provisioned_model_id);

    let model_arn = if model_id.contains(':') {
        model_id.to_string()
    } else {
        crate::arns::foundation_model_arn(&req.region, model_id)
    };

    let now = Utc::now();
    let throughput = ProvisionedThroughput {
        provisioned_model_id: provisioned_model_id.clone(),
        provisioned_model_arn: provisioned_model_arn.clone(),
        provisioned_model_name: provisioned_model_name.to_string(),
        model_arn,
        model_units,
        desired_model_units: model_units,
        status: "InService".to_string(),
        commitment_duration: body["commitmentDuration"].as_str().map(|s| s.to_string()),
        created_at: now,
        last_modified_at: now,
    };

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    s.provisioned_throughputs
        .insert(provisioned_model_id, throughput);

    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({
            "provisionedModelArn": provisioned_model_arn,
        }),
    ))
}

pub(crate) fn get_provisioned_model_throughput(
    state: &SharedBedrockState,
    req: &AwsRequest,
    provisioned_model_id: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let throughput = find_throughput(&s.provisioned_throughputs, provisioned_model_id)?;

    Ok(AwsResponse::ok_json(throughput_to_json(throughput)))
}

pub(crate) fn list_provisioned_model_throughputs(
    state: &SharedBedrockState,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let max_results = req
        .query_params
        .get("maxResults")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100)
        .max(1);
    let next_token = req.query_params.get("nextToken");

    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let mut items: Vec<&ProvisionedThroughput> = s.provisioned_throughputs.values().collect();
    items.sort_by(|a, b| a.provisioned_model_id.cmp(&b.provisioned_model_id));

    let start = if let Some(token) = next_token {
        items
            .iter()
            .position(|t| t.provisioned_model_id.as_str() > token.as_str())
            .unwrap_or(items.len())
    } else {
        0
    };

    let page: Vec<Value> = items
        .iter()
        .skip(start)
        .take(max_results)
        .map(|t| {
            json!({
                "provisionedModelName": t.provisioned_model_name,
                "provisionedModelArn": t.provisioned_model_arn,
                "modelArn": t.model_arn,
                "desiredModelArn": t.model_arn,
                "foundationModelArn": t.model_arn,
                "status": t.status,
                "modelUnits": t.model_units,
                "desiredModelUnits": t.desired_model_units,
                "creationTime": t.created_at.to_rfc3339(),
                "lastModifiedTime": t.last_modified_at.to_rfc3339(),
            })
        })
        .collect();

    let mut resp = json!({ "provisionedModelSummaries": page });
    let end = start.saturating_add(max_results);
    if end < items.len() {
        if let Some(last) = items.get(end - 1) {
            resp["nextToken"] = json!(last.provisioned_model_id);
        }
    }

    Ok(AwsResponse::ok_json(resp))
}

pub(crate) fn update_provisioned_model_throughput(
    state: &SharedBedrockState,
    req: &AwsRequest,
    provisioned_model_id: &str,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    let throughput = find_throughput_mut(&mut s.provisioned_throughputs, provisioned_model_id)?;

    if let Some(units) = body["desiredModelUnits"].as_i64() {
        throughput.desired_model_units = units as i32;
        throughput.model_units = units as i32;
    }
    if let Some(name) = body["desiredProvisionedModelName"].as_str() {
        throughput.provisioned_model_name = name.to_string();
    }
    throughput.last_modified_at = Utc::now();

    Ok(AwsResponse::json(StatusCode::OK, "{}".to_string()))
}

pub(crate) fn delete_provisioned_model_throughput(
    state: &SharedBedrockState,
    req: &AwsRequest,
    provisioned_model_id: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    // Find by ID or ARN
    let key = s
        .provisioned_throughputs
        .iter()
        .find(|(_, t)| {
            t.provisioned_model_id == provisioned_model_id
                || t.provisioned_model_arn == provisioned_model_id
        })
        .map(|(k, _)| k.clone());

    match key {
        Some(k) => {
            s.provisioned_throughputs.remove(&k);
            Ok(AwsResponse::json(StatusCode::OK, "{}".to_string()))
        }
        None => Err(AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Provisioned model {provisioned_model_id} not found"),
        )),
    }
}

fn find_throughput<'a>(
    throughputs: &'a std::collections::BTreeMap<String, ProvisionedThroughput>,
    id_or_arn: &str,
) -> Result<&'a ProvisionedThroughput, AwsServiceError> {
    throughputs
        .get(id_or_arn)
        .or_else(|| {
            throughputs
                .values()
                .find(|t| t.provisioned_model_arn == id_or_arn)
        })
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Provisioned model {id_or_arn} not found"),
            )
        })
}

fn find_throughput_mut<'a>(
    throughputs: &'a mut std::collections::BTreeMap<String, ProvisionedThroughput>,
    id_or_arn: &str,
) -> Result<&'a mut ProvisionedThroughput, AwsServiceError> {
    // First find the key
    let key = throughputs
        .iter()
        .find(|(k, t)| *k == id_or_arn || t.provisioned_model_arn == id_or_arn)
        .map(|(k, _)| k.clone())
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Provisioned model {id_or_arn} not found"),
            )
        })?;
    Ok(throughputs
        .get_mut(&key)
        .expect("key validated by find above"))
}

fn throughput_to_json(t: &ProvisionedThroughput) -> Value {
    json!({
        "provisionedModelName": t.provisioned_model_name,
        "provisionedModelArn": t.provisioned_model_arn,
        "modelArn": t.model_arn,
        "desiredModelArn": t.model_arn,
        "foundationModelArn": t.model_arn,
        "status": t.status,
        "modelUnits": t.model_units,
        "desiredModelUnits": t.desired_model_units,
        "commitmentDuration": t.commitment_duration,
        "creationTime": t.created_at.to_rfc3339(),
        "lastModifiedTime": t.last_modified_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::BedrockState;
    use bytes::Bytes;
    use fakecloud_core::multi_account::MultiAccountState;
    use http::{HeaderMap, Method};
    use parking_lot::RwLock;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn shared() -> SharedBedrockState {
        let multi: MultiAccountState<BedrockState> =
            MultiAccountState::new("123456789012", "us-east-1", "http://x");
        Arc::new(RwLock::new(multi))
    }

    fn req() -> AwsRequest {
        AwsRequest {
            service: "bedrock".to_string(),
            action: "a".to_string(),
            method: Method::POST,
            raw_path: "/".to_string(),
            raw_query: String::new(),
            path_segments: vec![],
            query_params: HashMap::new(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            body_stream: parking_lot::Mutex::new(None),
            account_id: "123456789012".to_string(),
            region: "us-east-1".to_string(),
            request_id: "r".to_string(),
            is_query_protocol: false,
            access_key_id: None,
            principal: None,
        }
    }

    fn create(
        state: &SharedBedrockState,
        name: &str,
        units: i64,
        duration: Option<&str>,
    ) -> String {
        let mut body = json!({
            "provisionedModelName": name,
            "modelId": "anthropic.claude",
            "modelUnits": units
        });
        if let Some(d) = duration {
            body["commitmentDuration"] = json!(d);
        }
        let resp = create_provisioned_model_throughput(state, &req(), &body).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        v["provisionedModelArn"].as_str().unwrap().to_string()
    }

    #[test]
    fn create_missing_name_errors() {
        let s = shared();
        let err = create_provisioned_model_throughput(&s, &req(), &json!({}))
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_zero_model_units_errors() {
        let s = shared();
        let err = create_provisioned_model_throughput(
            &s,
            &req(),
            &json!({"provisionedModelName": "p", "modelUnits": 0}),
        )
        .err()
        .unwrap();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_invalid_commitment_duration_errors() {
        let s = shared();
        let err = create_provisioned_model_throughput(
            &s,
            &req(),
            &json!({"provisionedModelName": "p", "commitmentDuration": "TwoYears"}),
        )
        .err()
        .unwrap();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_with_valid_commitment_duration() {
        let s = shared();
        let arn = create(&s, "p", 2, Some("SixMonths"));
        let accts = s.read();
        let state = accts.default_ref();
        let key = state
            .provisioned_throughputs
            .iter()
            .find(|(_, t)| t.provisioned_model_arn == arn)
            .unwrap()
            .0;
        assert_eq!(
            state.provisioned_throughputs[key]
                .commitment_duration
                .as_deref(),
            Some("SixMonths")
        );
    }

    #[test]
    fn create_builds_foundation_model_arn_when_no_colon() {
        let s = shared();
        let arn = create(&s, "p", 1, None);
        let state = s.read();
        let key = state
            .default_ref()
            .provisioned_throughputs
            .iter()
            .find(|(_, t)| t.provisioned_model_arn == arn)
            .unwrap()
            .0;
        assert!(state.default_ref().provisioned_throughputs[key]
            .model_arn
            .contains("foundation-model/"));
    }

    #[test]
    fn get_by_id_or_arn() {
        let s = shared();
        let arn = create(&s, "p", 1, None);
        let id = arn.rsplit('/').next().unwrap().to_string();
        assert!(get_provisioned_model_throughput(&s, &req(), &arn).is_ok());
        assert!(get_provisioned_model_throughput(&s, &req(), &id).is_ok());
    }

    #[test]
    fn get_unknown_returns_not_found() {
        let s = shared();
        let err = get_provisioned_model_throughput(&s, &req(), "missing")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_paginates() {
        let s = shared();
        for i in 0..3 {
            create(&s, &format!("p{i}"), 1, None);
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_provisioned_model_throughputs(&s, &r).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["provisionedModelSummaries"].as_array().unwrap().len(), 2);
        assert!(v["nextToken"].is_string());
    }

    #[test]
    fn update_changes_units_and_name() {
        let s = shared();
        let arn = create(&s, "p-old", 1, None);
        update_provisioned_model_throughput(
            &s,
            &req(),
            &arn,
            &json!({"desiredModelUnits": 5, "desiredProvisionedModelName": "p-new"}),
        )
        .unwrap();
        let state = s.read();
        let acct = state.default_ref();
        let t = acct
            .provisioned_throughputs
            .values()
            .find(|t| t.provisioned_model_arn == arn)
            .unwrap();
        assert_eq!(t.desired_model_units, 5);
        assert_eq!(t.model_units, 5);
        assert_eq!(t.provisioned_model_name, "p-new");
    }

    #[test]
    fn update_unknown_returns_not_found() {
        let s = shared();
        let err = update_provisioned_model_throughput(&s, &req(), "miss", &json!({}))
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn delete_removes_entry() {
        let s = shared();
        let arn = create(&s, "p", 1, None);
        delete_provisioned_model_throughput(&s, &req(), &arn).unwrap();
        assert!(s.read().default_ref().provisioned_throughputs.is_empty());
    }

    #[test]
    fn delete_unknown_returns_not_found() {
        let s = shared();
        let err = delete_provisioned_model_throughput(&s, &req(), "miss")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }
}
