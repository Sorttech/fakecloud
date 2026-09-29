use chrono::Utc;
use http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::{ModelCopyJob, SharedBedrockState};

pub(crate) fn create_model_copy_job(
    state: &SharedBedrockState,
    req: &AwsRequest,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let source_model_arn = body["sourceModelArn"].as_str().unwrap_or_default();
    let target_model_name = body["targetModelName"].as_str().unwrap_or("copied-model");

    let job_id = Uuid::new_v4().to_string();
    let job_arn = crate::arns::model_copy_job_arn(&req.region, &req.account_id, &job_id);
    let target_model_arn =
        crate::arns::custom_model_arn(&req.region, &req.account_id, target_model_name);

    let job = ModelCopyJob {
        job_arn: job_arn.clone(),
        source_model_arn: source_model_arn.to_string(),
        target_model_arn,
        target_model_name: target_model_name.to_string(),
        status: "Completed".to_string(),
        creation_time: Utc::now(),
    };

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    s.model_copy_jobs.insert(job_arn.clone(), job);

    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({ "jobArn": job_arn }),
    ))
}

pub(crate) fn get_model_copy_job(
    state: &SharedBedrockState,
    req: &AwsRequest,
    job_arn: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let job = s
        .model_copy_jobs
        .get(job_arn)
        .or_else(|| {
            s.model_copy_jobs
                .values()
                .find(|j| j.job_arn.ends_with(&format!("/{job_arn}")))
        })
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Model copy job {job_arn} not found"),
            )
        })?;

    Ok(AwsResponse::ok_json(json!({
        "jobArn": job.job_arn,
        "status": job.status,
        "creationTime": job.creation_time.to_rfc3339(),
        "sourceAccountId": source_account_id(&job.source_model_arn, &req.account_id),
        "sourceModelArn": job.source_model_arn,
        "targetModelArn": job.target_model_arn,
        "targetModelName": job.target_model_name,
    })))
}

pub(crate) fn list_model_copy_jobs(
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
    let mut items: Vec<&ModelCopyJob> = s.model_copy_jobs.values().collect();
    items.sort_by(|a, b| a.job_arn.cmp(&b.job_arn));

    let start = if let Some(token) = next_token {
        items
            .iter()
            .position(|j| j.job_arn.as_str() > token.as_str())
            .unwrap_or(items.len())
    } else {
        0
    };

    let page: Vec<Value> = items
        .iter()
        .skip(start)
        .take(max_results)
        .map(|j| {
            json!({
                "jobArn": j.job_arn,
                "status": j.status,
                "creationTime": j.creation_time.to_rfc3339(),
                "sourceAccountId": source_account_id(&j.source_model_arn, &req.account_id),
                "sourceModelArn": j.source_model_arn,
                "targetModelArn": j.target_model_arn,
                "targetModelName": j.target_model_name,
            })
        })
        .collect();

    let mut resp = json!({ "modelCopyJobSummaries": page });
    let end = start.saturating_add(max_results);
    if end < items.len() {
        if let Some(last) = items.get(end - 1) {
            resp["nextToken"] = json!(last.job_arn);
        }
    }

    Ok(AwsResponse::ok_json(resp))
}

/// Extract the AWS account ID from an ARN, falling back to the caller's
/// account when the ARN is malformed or missing.
fn source_account_id(source_model_arn: &str, fallback: &str) -> String {
    source_model_arn
        .split(':')
        .nth(4)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .to_string()
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

    fn create(state: &SharedBedrockState, target: &str) -> String {
        let body = json!({
            "sourceModelArn": "arn:aws:bedrock:us-east-1:123:custom-model/src",
            "targetModelName": target,
        });
        let resp = create_model_copy_job(state, &req(), &body).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        v["jobArn"].as_str().unwrap().to_string()
    }

    #[test]
    fn create_builds_target_model_arn() {
        let s = shared();
        let arn = create(&s, "cpy");
        let st = s.read();
        let job = st.default_ref().model_copy_jobs.get(&arn).unwrap();
        assert_eq!(job.target_model_name, "cpy");
        assert!(job.target_model_arn.ends_with("/cpy"));
    }

    #[test]
    fn get_by_full_arn_or_suffix() {
        let s = shared();
        let arn = create(&s, "t");
        let id = arn.rsplit('/').next().unwrap().to_string();
        assert!(get_model_copy_job(&s, &req(), &arn).is_ok());
        assert!(get_model_copy_job(&s, &req(), &id).is_ok());
    }

    #[test]
    fn get_unknown_returns_not_found() {
        let s = shared();
        let err = get_model_copy_job(&s, &req(), "missing").err().unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_paginates() {
        let s = shared();
        for i in 0..3 {
            create(&s, &format!("t{i}"));
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_model_copy_jobs(&s, &r).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["modelCopyJobSummaries"].as_array().unwrap().len(), 2);
        assert!(v["nextToken"].is_string());
    }
}
