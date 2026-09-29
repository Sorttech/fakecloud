use chrono::Utc;
use http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::{PromptRouter, SharedBedrockState};

pub(crate) fn create_prompt_router(
    state: &SharedBedrockState,
    req: &AwsRequest,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let default_name = crate::short_uuid();
    let router_name = body["promptRouterName"].as_str().unwrap_or(&default_name);

    let router_id = Uuid::new_v4().to_string();
    let router_arn = crate::arns::prompt_router_arn(&req.region, &req.account_id, &router_id);

    let now = Utc::now();
    let router = PromptRouter {
        prompt_router_arn: router_arn.clone(),
        prompt_router_name: router_name.to_string(),
        description: body["description"].as_str().map(|s| s.to_string()),
        models: body.get("models").cloned().unwrap_or(json!([])),
        routing_criteria: body.get("routingCriteria").cloned().unwrap_or(json!({})),
        fallback_model: body.get("fallbackModel").cloned().unwrap_or(json!({})),
        status: "Active".to_string(),
        prompt_router_type: "custom".to_string(),
        created_at: now,
        updated_at: now,
    };

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    s.prompt_routers.insert(router_arn.clone(), router);

    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({ "promptRouterArn": router_arn }),
    ))
}

pub(crate) fn get_prompt_router(
    state: &SharedBedrockState,
    req: &AwsRequest,
    identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let router = s
        .prompt_routers
        .get(identifier)
        .or_else(|| {
            s.prompt_routers.values().find(|r| {
                r.prompt_router_name == identifier
                    || r.prompt_router_arn.ends_with(&format!("/{identifier}"))
            })
        })
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Prompt router {identifier} not found"),
            )
        })?;

    Ok(AwsResponse::ok_json(json!({
        "promptRouterArn": router.prompt_router_arn,
        "promptRouterName": router.prompt_router_name,
        "description": router.description,
        "models": ensure_router_models(&router.models),
        "routingCriteria": ensure_routing_criteria(&router.routing_criteria),
        "fallbackModel": ensure_fallback_model(&router.fallback_model),
        "status": router.status,
        "type": router.prompt_router_type,
        "createdAt": router.created_at.to_rfc3339(),
        "updatedAt": router.updated_at.to_rfc3339(),
    })))
}

pub(crate) fn list_prompt_routers(
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
    let mut items: Vec<&PromptRouter> = s.prompt_routers.values().collect();
    items.sort_by(|a, b| a.prompt_router_arn.cmp(&b.prompt_router_arn));

    let start = if let Some(token) = next_token {
        items
            .iter()
            .position(|r| r.prompt_router_arn.as_str() > token.as_str())
            .unwrap_or(items.len())
    } else {
        0
    };

    let page: Vec<Value> = items
        .iter()
        .skip(start)
        .take(max_results)
        .map(|r| {
            json!({
                "promptRouterArn": r.prompt_router_arn,
                "promptRouterName": r.prompt_router_name,
                "description": r.description,
                "routingCriteria": ensure_routing_criteria(&r.routing_criteria),
                "models": ensure_router_models(&r.models),
                "fallbackModel": ensure_fallback_model(&r.fallback_model),
                "status": r.status,
                "type": r.prompt_router_type,
                "createdAt": r.created_at.to_rfc3339(),
                "updatedAt": r.updated_at.to_rfc3339(),
            })
        })
        .collect();

    let mut resp = json!({ "promptRouterSummaries": page });
    let end = start.saturating_add(max_results);
    if end < items.len() {
        if let Some(last) = items.get(end - 1) {
            resp["nextToken"] = json!(last.prompt_router_arn);
        }
    }

    Ok(AwsResponse::ok_json(resp))
}

/// Synthesize a minimal default `routingCriteria` when the router was created
/// without one, so the summary satisfies the Smithy required field.
fn ensure_routing_criteria(criteria: &Value) -> Value {
    if criteria.get("responseQualityDifference").is_some() {
        return criteria.clone();
    }
    json!({ "responseQualityDifference": 0.0 })
}

/// Default the router's target models to a single Titan entry when none were
/// supplied at create time, matching the required Smithy field.
fn ensure_router_models(models: &Value) -> Value {
    if let Some(arr) = models.as_array() {
        if !arr.is_empty() {
            return models.clone();
        }
    }
    json!([{
        "modelArn": "arn:aws:bedrock:us-east-1::foundation-model/amazon.titan-text-express-v1"
    }])
}

/// Default the fallback model when not provided. `fallbackModel` is required
/// in the summary even though it's optional on create.
fn ensure_fallback_model(fallback: &Value) -> Value {
    if fallback.get("modelArn").is_some() {
        return fallback.clone();
    }
    json!({
        "modelArn": "arn:aws:bedrock:us-east-1::foundation-model/amazon.titan-text-express-v1"
    })
}

pub(crate) fn delete_prompt_router(
    state: &SharedBedrockState,
    req: &AwsRequest,
    identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let key = s
        .prompt_routers
        .iter()
        .find(|(k, r)| {
            *k == identifier
                || r.prompt_router_name == identifier
                || r.prompt_router_arn.ends_with(&format!("/{identifier}"))
        })
        .map(|(k, _)| k.clone());

    match key {
        Some(k) => {
            s.prompt_routers.remove(&k);
            Ok(AwsResponse::json(StatusCode::OK, "{}".to_string()))
        }
        None => Err(AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Prompt router {identifier} not found"),
        )),
    }
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

    fn create(state: &SharedBedrockState, name: &str) -> String {
        let body = json!({
            "promptRouterName": name,
            "description": "d",
            "models": [{"modelArn": "m1"}],
            "routingCriteria": {"responseQualityDifference": 0.2},
            "fallbackModel": {"modelArn": "fallback"}
        });
        let resp = create_prompt_router(state, &req(), &body).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        v["promptRouterArn"].as_str().unwrap().to_string()
    }

    #[test]
    fn create_with_default_name_when_omitted() {
        let s = shared();
        create_prompt_router(&s, &req(), &json!({})).unwrap();
        let state = s.read();
        let acct = state.default_ref();
        assert_eq!(acct.prompt_routers.len(), 1);
    }

    #[test]
    fn get_by_arn_or_name_or_id() {
        let s = shared();
        let arn = create(&s, "my-router");
        let id = arn.rsplit('/').next().unwrap().to_string();
        assert!(get_prompt_router(&s, &req(), &arn).is_ok());
        assert!(get_prompt_router(&s, &req(), &id).is_ok());
        assert!(get_prompt_router(&s, &req(), "my-router").is_ok());
    }

    #[test]
    fn get_unknown_returns_not_found() {
        let s = shared();
        let err = get_prompt_router(&s, &req(), "missing").err().unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_paginates() {
        let s = shared();
        for i in 0..3 {
            create(&s, &format!("r{i}"));
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_prompt_routers(&s, &r).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["promptRouterSummaries"].as_array().unwrap().len(), 2);
        assert!(v["nextToken"].is_string());
    }

    #[test]
    fn delete_removes_entry() {
        let s = shared();
        let arn = create(&s, "del");
        delete_prompt_router(&s, &req(), &arn).unwrap();
        assert!(s.read().default_ref().prompt_routers.is_empty());
    }

    #[test]
    fn delete_unknown_returns_not_found() {
        let s = shared();
        let err = delete_prompt_router(&s, &req(), "missing").err().unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }
}
