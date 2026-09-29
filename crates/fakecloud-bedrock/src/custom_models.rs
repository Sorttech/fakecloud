use chrono::Utc;
use http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::{CustomModel, SharedBedrockState};

pub(crate) fn create_custom_model(
    state: &SharedBedrockState,
    req: &AwsRequest,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let default_name = crate::short_uuid();
    let model_name = body["modelName"].as_str().unwrap_or(&default_name);

    let model_id = Uuid::new_v4().to_string();
    let model_arn = crate::arns::custom_model_arn(&req.region, &req.account_id, &model_id);

    let base_model_name = body["baseModelIdentifier"]
        .as_str()
        .or_else(|| body["baseModelName"].as_str())
        .unwrap_or("amazon.titan-text-express-v1")
        .to_string();
    let base_model_arn = if base_model_name.starts_with("arn:") {
        base_model_name.clone()
    } else {
        crate::arns::foundation_model_arn(&req.region, &base_model_name)
    };

    let model = CustomModel {
        model_arn: model_arn.clone(),
        model_name: model_name.to_string(),
        base_model_arn,
        base_model_name,
        model_source_config: body.get("modelSourceConfig").cloned().unwrap_or(json!({})),
        model_kms_key_arn: body["modelKmsKeyArn"].as_str().map(|s| s.to_string()),
        role_arn: body["roleArn"].as_str().map(|s| s.to_string()),
        model_status: "Active".to_string(),
        creation_time: Utc::now(),
    };

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    s.custom_models.insert(model_arn.clone(), model);

    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({ "modelArn": model_arn }),
    ))
}

pub(crate) fn get_custom_model(
    state: &SharedBedrockState,
    req: &AwsRequest,
    model_identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let model = s
        .custom_models
        .get(model_identifier)
        .or_else(|| {
            s.custom_models.values().find(|m| {
                m.model_name == model_identifier
                    || m.model_arn.ends_with(&format!("/{model_identifier}"))
            })
        })
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Custom model {model_identifier} not found"),
            )
        })?;

    Ok(AwsResponse::ok_json(json!({
        "modelArn": model.model_arn,
        "modelName": model.model_name,
        "baseModelArn": model.base_model_arn,
        "modelStatus": model.model_status,
        "creationTime": model.creation_time.to_rfc3339(),
    })))
}

pub(crate) fn list_custom_models(
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
    let mut items: Vec<&CustomModel> = s.custom_models.values().collect();
    items.sort_by(|a, b| a.model_arn.cmp(&b.model_arn));

    let start = if let Some(token) = next_token {
        items
            .iter()
            .position(|m| m.model_arn.as_str() > token.as_str())
            .unwrap_or(items.len())
    } else {
        0
    };

    let page: Vec<Value> = items
        .iter()
        .skip(start)
        .take(max_results)
        .map(|m| {
            json!({
                "modelArn": m.model_arn,
                "modelName": m.model_name,
                "baseModelArn": m.base_model_arn,
                "baseModelName": m.base_model_name,
                "modelStatus": m.model_status,
                "creationTime": m.creation_time.to_rfc3339(),
            })
        })
        .collect();

    let mut resp = json!({ "modelSummaries": page });
    let end = start.saturating_add(max_results);
    if end < items.len() {
        if let Some(last) = items.get(end - 1) {
            resp["nextToken"] = json!(last.model_arn);
        }
    }

    Ok(AwsResponse::ok_json(resp))
}

pub(crate) fn delete_custom_model(
    state: &SharedBedrockState,
    req: &AwsRequest,
    model_identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let key = s
        .custom_models
        .iter()
        .find(|(k, m)| {
            *k == model_identifier
                || m.model_name == model_identifier
                || m.model_arn.ends_with(&format!("/{model_identifier}"))
        })
        .map(|(k, _)| k.clone());

    match key {
        Some(k) => {
            s.custom_models.remove(&k);
            Ok(AwsResponse::json(StatusCode::OK, "{}".to_string()))
        }
        None => Err(AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Custom model {model_identifier} not found"),
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
            "modelName": name,
            "modelSourceConfig": {"s3DataSource": {"s3Uri": "s3://b/m"}},
            "modelKmsKeyArn": "arn:aws:kms:us-east-1:123:key/k",
            "roleArn": "arn:aws:iam::123:role/r"
        });
        let resp = create_custom_model(state, &req(), &body).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        v["modelArn"].as_str().unwrap().to_string()
    }

    #[test]
    fn create_with_default_name_when_omitted() {
        let s = shared();
        create_custom_model(&s, &req(), &json!({})).unwrap();
        let st = s.read();
        assert_eq!(st.default_ref().custom_models.len(), 1);
    }

    #[test]
    fn get_by_arn_or_name_or_id() {
        let s = shared();
        let arn = create(&s, "my-model");
        let id = arn.rsplit('/').next().unwrap().to_string();
        assert!(get_custom_model(&s, &req(), &arn).is_ok());
        assert!(get_custom_model(&s, &req(), &id).is_ok());
        assert!(get_custom_model(&s, &req(), "my-model").is_ok());
    }

    #[test]
    fn get_unknown_returns_not_found() {
        let s = shared();
        let err = get_custom_model(&s, &req(), "missing").err().unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_paginates() {
        let s = shared();
        for i in 0..3 {
            create(&s, &format!("m{i}"));
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_custom_models(&s, &r).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["modelSummaries"].as_array().unwrap().len(), 2);
        assert!(v["nextToken"].is_string());
    }

    #[test]
    fn delete_removes_entry() {
        let s = shared();
        let arn = create(&s, "del");
        delete_custom_model(&s, &req(), &arn).unwrap();
        assert!(s.read().default_ref().custom_models.is_empty());
    }

    #[test]
    fn delete_unknown_returns_not_found() {
        let s = shared();
        let err = delete_custom_model(&s, &req(), "missing").err().unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }
}
