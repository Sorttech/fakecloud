use chrono::Utc;
use http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::{ModelInvocationJob, SharedBedrockState};

pub(crate) fn create_model_invocation_job(
    state: &SharedBedrockState,
    req: &AwsRequest,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let job_name = body["jobName"].as_str().unwrap_or("invocation-job");
    let model_id = body["modelId"].as_str().unwrap_or_default();
    let role_arn = body["roleArn"].as_str().unwrap_or_default();

    let job_id = Uuid::new_v4().to_string();
    let job_arn = crate::arns::model_invocation_job_arn(&req.region, &req.account_id, &job_id);

    let now = Utc::now();
    let job = ModelInvocationJob {
        job_arn: job_arn.clone(),
        job_name: job_name.to_string(),
        model_id: model_id.to_string(),
        role_arn: role_arn.to_string(),
        input_data_config: body.get("inputDataConfig").cloned().unwrap_or(json!({})),
        output_data_config: body.get("outputDataConfig").cloned().unwrap_or(json!({})),
        status: "InProgress".to_string(),
        submit_time: now,
        last_modified_time: now,
        end_time: None,
    };

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    s.model_invocation_jobs.insert(job_arn.clone(), job);

    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({ "jobArn": job_arn }),
    ))
}

pub(crate) fn get_model_invocation_job(
    state: &SharedBedrockState,
    req: &AwsRequest,
    job_identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let job = find_job(&s.model_invocation_jobs, job_identifier)?;

    let mut resp = json!({
        "jobArn": job.job_arn,
        "jobName": job.job_name,
        "modelId": job.model_id,
        "roleArn": job.role_arn,
        "status": job.status,
        "inputDataConfig": ensure_input_data_config(&job.input_data_config),
        "outputDataConfig": ensure_output_data_config(&job.output_data_config),
        "submitTime": job.submit_time.to_rfc3339(),
        "lastModifiedTime": job.last_modified_time.to_rfc3339(),
    });
    if let Some(ref end_time) = job.end_time {
        resp["endTime"] = json!(end_time.to_rfc3339());
    }

    Ok(AwsResponse::ok_json(resp))
}

pub(crate) fn list_model_invocation_jobs(
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
    let mut items: Vec<&ModelInvocationJob> = s.model_invocation_jobs.values().collect();
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
                "jobName": j.job_name,
                "modelId": j.model_id,
                "roleArn": j.role_arn,
                "status": j.status,
                "submitTime": j.submit_time.to_rfc3339(),
                "lastModifiedTime": j.last_modified_time.to_rfc3339(),
                "inputDataConfig": ensure_input_data_config(&j.input_data_config),
                "outputDataConfig": ensure_output_data_config(&j.output_data_config),
            })
        })
        .collect();

    let mut resp = json!({ "invocationJobSummaries": page });
    let end = start.saturating_add(max_results);
    if end < items.len() {
        if let Some(last) = items.get(end - 1) {
            resp["nextToken"] = json!(last.job_arn);
        }
    }

    Ok(AwsResponse::ok_json(resp))
}

pub(crate) fn stop_model_invocation_job(
    state: &SharedBedrockState,
    req: &AwsRequest,
    job_identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    let key = find_job_key(&s.model_invocation_jobs, job_identifier)?;
    let job = s
        .model_invocation_jobs
        .get_mut(&key)
        .expect("key validated by find_job_key");

    if job.status != "InProgress" {
        return Err(AwsServiceError::aws_error(
            StatusCode::CONFLICT,
            "ConflictException",
            format!("Job is not in InProgress status (current: {})", job.status),
        ));
    }

    let now = Utc::now();
    job.status = "Stopped".to_string();
    job.last_modified_time = now;
    job.end_time = Some(now);

    Ok(AwsResponse::json(StatusCode::OK, "{}".to_string()))
}

fn ensure_input_data_config(cfg: &Value) -> Value {
    if cfg.get("s3InputDataConfig").is_some() {
        return cfg.clone();
    }
    json!({
        "s3InputDataConfig": {
            "s3Uri": "s3://fakecloud-bedrock-batch/input/"
        }
    })
}

fn ensure_output_data_config(cfg: &Value) -> Value {
    if cfg.get("s3OutputDataConfig").is_some() {
        return cfg.clone();
    }
    json!({
        "s3OutputDataConfig": {
            "s3Uri": "s3://fakecloud-bedrock-batch/output/"
        }
    })
}

fn find_job<'a>(
    jobs: &'a std::collections::BTreeMap<String, ModelInvocationJob>,
    id_or_arn: &str,
) -> Result<&'a ModelInvocationJob, AwsServiceError> {
    jobs.get(id_or_arn)
        .or_else(|| {
            jobs.values()
                .find(|j| j.job_name == id_or_arn || j.job_arn.ends_with(&format!("/{id_or_arn}")))
        })
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Model invocation job {id_or_arn} not found"),
            )
        })
}

fn find_job_key(
    jobs: &std::collections::BTreeMap<String, ModelInvocationJob>,
    id_or_arn: &str,
) -> Result<String, AwsServiceError> {
    jobs.iter()
        .find(|(k, j)| {
            *k == id_or_arn
                || j.job_name == id_or_arn
                || j.job_arn.ends_with(&format!("/{id_or_arn}"))
        })
        .map(|(k, _)| k.clone())
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Model invocation job {id_or_arn} not found"),
            )
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
        let body = json!({
            "jobName": name,
            "modelId": "anthropic.claude",
            "roleArn": "arn:aws:iam::123:role/r",
            "inputDataConfig": {"s3InputDataConfig": {"s3Uri": "s3://b/in"}},
            "outputDataConfig": {"s3OutputDataConfig": {"s3Uri": "s3://b/out"}}
        });
        let resp = create_model_invocation_job(state, &req(), &body).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        v["jobArn"].as_str().unwrap().to_string()
    }

    #[test]
    fn create_stores_job() {
        let s = shared();
        let arn = create(&s, "j1");
        let st = s.read();
        let job = st.default_ref().model_invocation_jobs.get(&arn).unwrap();
        assert_eq!(job.job_name, "j1");
        assert_eq!(job.status, "InProgress");
    }

    #[test]
    fn get_by_arn_name_or_id() {
        let s = shared();
        let arn = create(&s, "my-job");
        let id = arn.rsplit('/').next().unwrap().to_string();
        assert!(get_model_invocation_job(&s, &req(), &arn).is_ok());
        assert!(get_model_invocation_job(&s, &req(), &id).is_ok());
        assert!(get_model_invocation_job(&s, &req(), "my-job").is_ok());
    }

    #[test]
    fn get_unknown_returns_not_found() {
        let s = shared();
        let err = get_model_invocation_job(&s, &req(), "missing")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_paginates() {
        let s = shared();
        for i in 0..3 {
            create(&s, &format!("j{i}"));
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_model_invocation_jobs(&s, &r).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["invocationJobSummaries"].as_array().unwrap().len(), 2);
        assert!(v["nextToken"].is_string());
    }

    #[test]
    fn stop_sets_status_and_end_time() {
        let s = shared();
        let arn = create(&s, "j");
        stop_model_invocation_job(&s, &req(), &arn).unwrap();
        let st = s.read();
        let job = st.default_ref().model_invocation_jobs.get(&arn).unwrap();
        assert_eq!(job.status, "Stopped");
        assert!(job.end_time.is_some());
    }

    #[test]
    fn stop_rejects_non_in_progress() {
        let s = shared();
        let arn = create(&s, "j");
        stop_model_invocation_job(&s, &req(), &arn).unwrap();
        let err = stop_model_invocation_job(&s, &req(), &arn).err().unwrap();
        assert_eq!(err.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn stop_unknown_returns_not_found() {
        let s = shared();
        let err = stop_model_invocation_job(&s, &req(), "missing")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }
}
