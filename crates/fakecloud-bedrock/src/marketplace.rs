use chrono::Utc;
use http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::{MarketplaceModelEndpoint, SharedBedrockState};

pub(crate) fn create_marketplace_model_endpoint(
    state: &SharedBedrockState,
    req: &AwsRequest,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let endpoint_name = body["endpointName"].as_str().ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "endpointName is required",
        )
    })?;

    let model_source_identifier = body["modelSourceIdentifier"].as_str().ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "modelSourceIdentifier is required",
        )
    })?;

    let endpoint_id = Uuid::new_v4().to_string();
    let endpoint_arn =
        crate::arns::marketplace_model_endpoint_arn(&req.region, &req.account_id, &endpoint_id);

    let now = Utc::now();
    let endpoint = MarketplaceModelEndpoint {
        endpoint_arn: endpoint_arn.clone(),
        endpoint_name: endpoint_name.to_string(),
        model_source_identifier: model_source_identifier.to_string(),
        status: "Active".to_string(),
        endpoint_config: body.get("endpointConfig").cloned().unwrap_or(json!({})),
        created_at: now,
        updated_at: now,
    };

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    s.marketplace_endpoints
        .insert(endpoint_arn.clone(), endpoint);

    let saved = s
        .marketplace_endpoints
        .get(&endpoint_arn)
        .expect("just inserted");
    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({ "marketplaceModelEndpoint": endpoint_to_json(saved) }),
    ))
}

/// Build the canonical `MarketplaceModelEndpoint` shape that the Smithy model
/// expects. Note: per the model `endpointStatus` is required while
/// `endpointName` is not a member — we surface the user-supplied name through
/// `endpointConfig.endpointName` only when callers later need it.
fn endpoint_to_json(e: &MarketplaceModelEndpoint) -> Value {
    json!({
        "endpointArn": e.endpoint_arn,
        "modelSourceIdentifier": e.model_source_identifier,
        "status": e.status,
        "endpointStatus": endpoint_status_for(&e.status),
        "endpointConfig": e.endpoint_config,
        "createdAt": e.created_at.to_rfc3339(),
        "updatedAt": e.updated_at.to_rfc3339(),
    })
}

fn endpoint_status_for(status: &str) -> &'static str {
    match status {
        "Registered" => "InService",
        "Updating" => "Updating",
        "Failed" => "Failed",
        _ => "InService",
    }
}

pub(crate) fn get_marketplace_model_endpoint(
    state: &SharedBedrockState,
    req: &AwsRequest,
    identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let endpoint = s
        .marketplace_endpoints
        .get(identifier)
        .or_else(|| {
            s.marketplace_endpoints.values().find(|e| {
                e.endpoint_name == identifier || e.endpoint_arn.ends_with(&format!("/{identifier}"))
            })
        })
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Marketplace model endpoint {identifier} not found"),
            )
        })?;

    Ok(AwsResponse::ok_json(json!({
        "marketplaceModelEndpoint": endpoint_to_json(endpoint),
    })))
}

pub(crate) fn list_marketplace_model_endpoints(
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
    let mut items: Vec<&MarketplaceModelEndpoint> = s.marketplace_endpoints.values().collect();
    items.sort_by(|a, b| a.endpoint_arn.cmp(&b.endpoint_arn));

    let start = if let Some(token) = next_token {
        items
            .iter()
            .position(|e| e.endpoint_arn.as_str() > token.as_str())
            .unwrap_or(items.len())
    } else {
        0
    };

    let page: Vec<Value> = items
        .iter()
        .skip(start)
        .take(max_results)
        .map(|e| {
            json!({
                "endpointArn": e.endpoint_arn,
                "modelSourceIdentifier": e.model_source_identifier,
                "status": e.status,
                "createdAt": e.created_at.to_rfc3339(),
                "updatedAt": e.updated_at.to_rfc3339(),
            })
        })
        .collect();

    let mut resp = json!({ "marketplaceModelEndpoints": page });
    let end = start.saturating_add(max_results);
    if end < items.len() {
        if let Some(last) = items.get(end - 1) {
            resp["nextToken"] = json!(last.endpoint_arn);
        }
    }

    Ok(AwsResponse::ok_json(resp))
}

pub(crate) fn update_marketplace_model_endpoint(
    state: &SharedBedrockState,
    req: &AwsRequest,
    identifier: &str,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let key = s
        .marketplace_endpoints
        .iter()
        .find(|(k, e)| {
            *k == identifier
                || e.endpoint_name == identifier
                || e.endpoint_arn.ends_with(&format!("/{identifier}"))
        })
        .map(|(k, _)| k.clone())
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Marketplace model endpoint {identifier} not found"),
            )
        })?;

    let endpoint = s
        .marketplace_endpoints
        .get_mut(&key)
        .expect("key validated by find above");

    if let Some(config) = body.get("endpointConfig") {
        endpoint.endpoint_config = config.clone();
    }
    endpoint.updated_at = Utc::now();

    Ok(AwsResponse::ok_json(json!({
        "marketplaceModelEndpoint": endpoint_to_json(endpoint),
    })))
}

pub(crate) fn delete_marketplace_model_endpoint(
    state: &SharedBedrockState,
    req: &AwsRequest,
    identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let key = s
        .marketplace_endpoints
        .iter()
        .find(|(k, e)| {
            *k == identifier
                || e.endpoint_name == identifier
                || e.endpoint_arn.ends_with(&format!("/{identifier}"))
        })
        .map(|(k, _)| k.clone());

    match key {
        Some(k) => {
            s.marketplace_endpoints.remove(&k);
            Ok(AwsResponse::json(StatusCode::OK, "{}".to_string()))
        }
        None => Err(AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Marketplace model endpoint {identifier} not found"),
        )),
    }
}

pub(crate) fn register_marketplace_model_endpoint(
    state: &SharedBedrockState,
    req: &AwsRequest,
    identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let key = s
        .marketplace_endpoints
        .iter()
        .find(|(k, e)| {
            *k == identifier
                || e.endpoint_name == identifier
                || e.endpoint_arn.ends_with(&format!("/{identifier}"))
        })
        .map(|(k, _)| k.clone())
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Marketplace model endpoint {identifier} not found"),
            )
        })?;

    let endpoint = s
        .marketplace_endpoints
        .get_mut(&key)
        .expect("key validated by find above");
    endpoint.status = "Registered".to_string();
    endpoint.updated_at = Utc::now();

    Ok(AwsResponse::ok_json(json!({
        "marketplaceModelEndpoint": endpoint_to_json(endpoint),
    })))
}

pub(crate) fn deregister_marketplace_model_endpoint(
    state: &SharedBedrockState,
    req: &AwsRequest,
    identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let key = s
        .marketplace_endpoints
        .iter()
        .find(|(k, e)| {
            *k == identifier
                || e.endpoint_name == identifier
                || e.endpoint_arn.ends_with(&format!("/{identifier}"))
        })
        .map(|(k, _)| k.clone())
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Marketplace model endpoint {identifier} not found"),
            )
        })?;

    let endpoint = s
        .marketplace_endpoints
        .get_mut(&key)
        .expect("key validated by find above");
    endpoint.status = "Active".to_string();
    endpoint.updated_at = Utc::now();

    Ok(AwsResponse::ok_json(json!({})))
}
#[cfg(test)]
#[allow(clippy::too_many_lines)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::{HeaderMap, Method};
    use parking_lot::RwLock;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn shared() -> SharedBedrockState {
        Arc::new(RwLock::new(
            fakecloud_core::multi_account::MultiAccountState::new(
                "123456789012",
                "us-east-1",
                "http://localhost:4566",
            ),
        ))
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
            request_id: "req".to_string(),
            is_query_protocol: false,
            access_key_id: None,
            principal: None,
        }
    }

    fn create(state: &SharedBedrockState, name: &str) -> String {
        let body = json!({"endpointName": name, "modelSourceIdentifier": "m"});
        let resp = create_marketplace_model_endpoint(state, &req(), &body).unwrap();
        let text = std::str::from_utf8(resp.body.expect_bytes()).unwrap();
        let v: Value = serde_json::from_str(text).unwrap();
        v["marketplaceModelEndpoint"]["endpointArn"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn create_missing_name_errors() {
        let s = shared();
        let err =
            create_marketplace_model_endpoint(&s, &req(), &json!({"modelSourceIdentifier": "m"}))
                .err()
                .unwrap();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_missing_model_source_errors() {
        let s = shared();
        let err = create_marketplace_model_endpoint(&s, &req(), &json!({"endpointName": "n"}))
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_and_get_by_arn() {
        let s = shared();
        let arn = create(&s, "ep-1");
        let resp = get_marketplace_model_endpoint(&s, &req(), &arn).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["marketplaceModelEndpoint"]["endpointArn"], arn);
        assert_eq!(v["marketplaceModelEndpoint"]["endpointStatus"], "InService");
    }

    #[test]
    fn get_by_name_or_id() {
        let s = shared();
        let arn = create(&s, "my-ep");
        let id = arn.rsplit('/').next().unwrap().to_string();
        assert!(get_marketplace_model_endpoint(&s, &req(), &id).is_ok());
        assert!(get_marketplace_model_endpoint(&s, &req(), "my-ep").is_ok());
    }

    #[test]
    fn get_unknown_returns_not_found() {
        let s = shared();
        let err = get_marketplace_model_endpoint(&s, &req(), "missing")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_paginates_over_endpoints() {
        let s = shared();
        for i in 0..3 {
            create(&s, &format!("ep-{i}"));
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_marketplace_model_endpoints(&s, &r).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["marketplaceModelEndpoints"].as_array().unwrap().len(), 2);
        assert!(v["nextToken"].is_string());
    }

    #[test]
    fn update_sets_new_config() {
        let s = shared();
        let arn = create(&s, "up-ep");
        update_marketplace_model_endpoint(&s, &req(), &arn, &json!({"endpointConfig": {"a": 1}}))
            .unwrap();
        assert_eq!(
            s.read().default_ref().marketplace_endpoints[&arn].endpoint_config,
            json!({"a": 1})
        );
    }

    #[test]
    fn update_unknown_returns_not_found() {
        let s = shared();
        let err = update_marketplace_model_endpoint(&s, &req(), "miss", &json!({}))
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn delete_removes_endpoint() {
        let s = shared();
        let arn = create(&s, "del-ep");
        delete_marketplace_model_endpoint(&s, &req(), &arn).unwrap();
        assert!(s.read().default_ref().marketplace_endpoints.is_empty());
    }

    #[test]
    fn delete_unknown_returns_not_found() {
        let s = shared();
        let err = delete_marketplace_model_endpoint(&s, &req(), "miss")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn register_transitions_status() {
        let s = shared();
        let arn = create(&s, "reg-ep");
        register_marketplace_model_endpoint(&s, &req(), &arn).unwrap();
        assert_eq!(
            s.read().default_ref().marketplace_endpoints[&arn].status,
            "Registered"
        );
    }

    #[test]
    fn register_unknown_returns_not_found() {
        let s = shared();
        let err = register_marketplace_model_endpoint(&s, &req(), "miss")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn deregister_resets_to_active() {
        let s = shared();
        let arn = create(&s, "dereg");
        register_marketplace_model_endpoint(&s, &req(), &arn).unwrap();
        deregister_marketplace_model_endpoint(&s, &req(), &arn).unwrap();
        assert_eq!(
            s.read().default_ref().marketplace_endpoints[&arn].status,
            "Active"
        );
    }

    #[test]
    fn deregister_unknown_returns_not_found() {
        let s = shared();
        let err = deregister_marketplace_model_endpoint(&s, &req(), "miss")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }
}
