use chrono::Utc;
use http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::{CustomModelDeployment, SharedBedrockState};

pub(crate) fn create_custom_model_deployment(
    state: &SharedBedrockState,
    req: &AwsRequest,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let default_name = format!("deployment-{}", &Uuid::new_v4().to_string()[..8]);
    let deployment_name = body["modelDeploymentName"]
        .as_str()
        .unwrap_or(&default_name);
    let model_arn = body["modelArn"].as_str().unwrap_or_default();

    let deployment_id = Uuid::new_v4().to_string();
    let deployment_arn =
        crate::arns::custom_model_deployment_arn(&req.region, &req.account_id, &deployment_id);

    let now = Utc::now();
    let deployment = CustomModelDeployment {
        deployment_arn: deployment_arn.clone(),
        deployment_name: deployment_name.to_string(),
        model_arn: model_arn.to_string(),
        description: body["description"].as_str().map(|s| s.to_string()),
        status: "Active".to_string(),
        created_at: now,
        last_updated_at: now,
    };

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    s.custom_model_deployments
        .insert(deployment_arn.clone(), deployment);

    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({ "customModelDeploymentArn": deployment_arn }),
    ))
}

pub(crate) fn get_custom_model_deployment(
    state: &SharedBedrockState,
    req: &AwsRequest,
    deployment_identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let deployment = find_deployment(&s.custom_model_deployments, deployment_identifier)?;

    Ok(AwsResponse::ok_json(deployment_to_json(deployment)))
}

pub(crate) fn list_custom_model_deployments(
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
    let mut items: Vec<&CustomModelDeployment> = s.custom_model_deployments.values().collect();
    items.sort_by(|a, b| a.deployment_arn.cmp(&b.deployment_arn));

    let start = if let Some(token) = next_token {
        items
            .iter()
            .position(|d| d.deployment_arn.as_str() > token.as_str())
            .unwrap_or(items.len())
    } else {
        0
    };

    let page: Vec<Value> = items
        .iter()
        .skip(start)
        .take(max_results)
        .map(|d| deployment_summary_json(d))
        .collect();

    let mut resp = json!({ "modelDeploymentSummaries": page });
    let end = start.saturating_add(max_results);
    if end < items.len() {
        if let Some(last) = items.get(end - 1) {
            resp["nextToken"] = json!(last.deployment_arn);
        }
    }

    Ok(AwsResponse::ok_json(resp))
}

pub(crate) fn update_custom_model_deployment(
    state: &SharedBedrockState,
    req: &AwsRequest,
    deployment_identifier: &str,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    let key = find_deployment_key(&s.custom_model_deployments, deployment_identifier)?;
    let deployment = s
        .custom_model_deployments
        .get_mut(&key)
        .expect("key validated by find_deployment_key");

    if let Some(model_arn) = body["modelArn"].as_str() {
        deployment.model_arn = model_arn.to_string();
    }
    deployment.last_updated_at = Utc::now();

    let arn = deployment.deployment_arn.clone();
    Ok(AwsResponse::ok_json(
        json!({ "customModelDeploymentArn": arn }),
    ))
}

pub(crate) fn delete_custom_model_deployment(
    state: &SharedBedrockState,
    req: &AwsRequest,
    deployment_identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    let key = find_deployment_key(&s.custom_model_deployments, deployment_identifier)?;
    s.custom_model_deployments.remove(&key);
    Ok(AwsResponse::json(StatusCode::OK, "{}".to_string()))
}

fn find_deployment<'a>(
    deployments: &'a std::collections::BTreeMap<String, CustomModelDeployment>,
    id_or_arn: &str,
) -> Result<&'a CustomModelDeployment, AwsServiceError> {
    deployments
        .get(id_or_arn)
        .or_else(|| {
            deployments.values().find(|d| {
                d.deployment_name == id_or_arn
                    || d.deployment_arn.ends_with(&format!("/{id_or_arn}"))
            })
        })
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Custom model deployment {id_or_arn} not found"),
            )
        })
}

fn find_deployment_key(
    deployments: &std::collections::BTreeMap<String, CustomModelDeployment>,
    id_or_arn: &str,
) -> Result<String, AwsServiceError> {
    deployments
        .iter()
        .find(|(k, d)| {
            *k == id_or_arn
                || d.deployment_name == id_or_arn
                || d.deployment_arn.ends_with(&format!("/{id_or_arn}"))
        })
        .map(|(k, _)| k.clone())
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Custom model deployment {id_or_arn} not found"),
            )
        })
}

fn deployment_to_json(d: &CustomModelDeployment) -> Value {
    let mut obj = json!({
        "customModelDeploymentArn": d.deployment_arn,
        "modelDeploymentName": d.deployment_name,
        "modelArn": d.model_arn,
        "status": d.status,
        "createdAt": d.created_at.to_rfc3339(),
        "lastUpdatedAt": d.last_updated_at.to_rfc3339(),
    });
    if let Some(ref desc) = d.description {
        obj["description"] = json!(desc);
    }
    obj
}

fn deployment_summary_json(d: &CustomModelDeployment) -> Value {
    // CustomModelDeploymentSummary intentionally omits `description` per the
    // Smithy model; deployment description only lives on the full GET shape.
    json!({
        "customModelDeploymentArn": d.deployment_arn,
        "customModelDeploymentName": d.deployment_name,
        "modelArn": d.model_arn,
        "status": d.status,
        "createdAt": d.created_at.to_rfc3339(),
        "lastUpdatedAt": d.last_updated_at.to_rfc3339(),
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

    fn create(state: &SharedBedrockState, name: &str) -> String {
        let resp = create_custom_model_deployment(
            state,
            &req(),
            &json!({
                "modelDeploymentName": name,
                "modelArn": "arn:aws:bedrock:us-east-1:123:custom-model/m",
                "description": "desc"
            }),
        )
        .unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        v["customModelDeploymentArn"].as_str().unwrap().to_string()
    }

    #[test]
    fn create_default_name_when_omitted() {
        let s = shared();
        let resp =
            create_custom_model_deployment(&s, &req(), &json!({"modelArn": "arn-x"})).unwrap();
        assert_eq!(resp.status, StatusCode::CREATED);
        let state = s.read();
        let acct = state.default_ref();
        let dep = acct.custom_model_deployments.values().next().unwrap();
        assert!(dep.deployment_name.starts_with("deployment-"));
    }

    #[test]
    fn get_by_arn_or_name_or_id() {
        let s = shared();
        let arn = create(&s, "my-deploy");
        let id = arn.rsplit('/').next().unwrap().to_string();
        assert!(get_custom_model_deployment(&s, &req(), &arn).is_ok());
        assert!(get_custom_model_deployment(&s, &req(), &id).is_ok());
        assert!(get_custom_model_deployment(&s, &req(), "my-deploy").is_ok());
    }

    #[test]
    fn get_unknown_returns_not_found() {
        let s = shared();
        let err = get_custom_model_deployment(&s, &req(), "missing")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_paginates() {
        let s = shared();
        for i in 0..3 {
            create(&s, &format!("d{i}"));
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_custom_model_deployments(&s, &r).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["modelDeploymentSummaries"].as_array().unwrap().len(), 2);
        assert!(v["nextToken"].is_string());
    }

    #[test]
    fn update_changes_model_arn() {
        let s = shared();
        let arn = create(&s, "up");
        update_custom_model_deployment(&s, &req(), &arn, &json!({"modelArn": "new-arn"})).unwrap();
        let state = s.read();
        let d = state
            .default_ref()
            .custom_model_deployments
            .get(&arn)
            .unwrap();
        assert_eq!(d.model_arn, "new-arn");
    }

    #[test]
    fn update_unknown_returns_not_found() {
        let s = shared();
        let err = update_custom_model_deployment(&s, &req(), "missing", &json!({"modelArn": "x"}))
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn delete_removes_deployment() {
        let s = shared();
        let arn = create(&s, "del");
        delete_custom_model_deployment(&s, &req(), &arn).unwrap();
        assert!(s.read().default_ref().custom_model_deployments.is_empty());
    }

    #[test]
    fn delete_unknown_returns_not_found() {
        let s = shared();
        let err = delete_custom_model_deployment(&s, &req(), "missing")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }
}
