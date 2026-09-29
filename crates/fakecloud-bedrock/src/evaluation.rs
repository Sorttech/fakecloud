use chrono::Utc;
use http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::{EvaluationJob, SharedBedrockState};

pub(crate) fn create_evaluation_job(
    state: &SharedBedrockState,
    req: &AwsRequest,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let job_name = body["jobName"].as_str().unwrap_or("eval-job");
    let role_arn = body["roleArn"].as_str().unwrap_or_default();

    let job_id = Uuid::new_v4().to_string();
    let job_arn = crate::arns::evaluation_job_arn(&req.region, &req.account_id, &job_id);

    let now = Utc::now();
    let job = EvaluationJob {
        job_arn: job_arn.clone(),
        job_name: job_name.to_string(),
        job_description: body["jobDescription"].as_str().map(|s| s.to_string()),
        role_arn: role_arn.to_string(),
        status: "InProgress".to_string(),
        job_type: body["evaluationConfig"]["automated"]
            .as_object()
            .map(|_| "Automated")
            .unwrap_or("Human")
            .to_string(),
        evaluation_config: body.get("evaluationConfig").cloned().unwrap_or(json!({})),
        inference_config: body.get("inferenceConfig").cloned().unwrap_or(json!({})),
        output_data_config: body.get("outputDataConfig").cloned().unwrap_or(json!({})),
        creation_time: now,
        last_modified_time: now,
    };

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    s.evaluation_jobs.insert(job_arn.clone(), job);

    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({ "jobArn": job_arn }),
    ))
}

pub(crate) fn get_evaluation_job(
    state: &SharedBedrockState,
    req: &AwsRequest,
    job_identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let job = find_job(&s.evaluation_jobs, job_identifier)?;

    Ok(AwsResponse::ok_json(json!({
        "jobArn": job.job_arn,
        "jobName": job.job_name,
        "jobDescription": job.job_description,
        "roleArn": job.role_arn,
        "status": job.status,
        "jobType": job.job_type,
        "evaluationConfig": job.evaluation_config,
        "inferenceConfig": job.inference_config,
        "outputDataConfig": job.output_data_config,
        "creationTime": job.creation_time.to_rfc3339(),
        "lastModifiedTime": job.last_modified_time.to_rfc3339(),
    })))
}

pub(crate) fn list_evaluation_jobs(
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
    let mut items: Vec<&EvaluationJob> = s.evaluation_jobs.values().collect();
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
                "status": j.status,
                "jobType": j.job_type,
                "creationTime": j.creation_time.to_rfc3339(),
                "evaluationTaskTypes": ["Generation"],
            })
        })
        .collect();

    let mut resp = json!({ "jobSummaries": page });
    let end = start.saturating_add(max_results);
    if end < items.len() {
        if let Some(last) = items.get(end - 1) {
            resp["nextToken"] = json!(last.job_arn);
        }
    }

    Ok(AwsResponse::ok_json(resp))
}

pub(crate) fn stop_evaluation_job(
    state: &SharedBedrockState,
    req: &AwsRequest,
    job_identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    let key = find_job_key(&s.evaluation_jobs, job_identifier)?;
    let job = s
        .evaluation_jobs
        .get_mut(&key)
        .expect("key validated by find_job_key");

    if job.status != "InProgress" {
        return Err(AwsServiceError::aws_error(
            StatusCode::CONFLICT,
            "ConflictException",
            format!("Job is not in InProgress status (current: {})", job.status),
        ));
    }

    job.status = "Stopped".to_string();
    job.last_modified_time = Utc::now();

    Ok(AwsResponse::json(StatusCode::OK, "{}".to_string()))
}

pub(crate) fn batch_delete_evaluation_job(
    state: &SharedBedrockState,
    req: &AwsRequest,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let job_identifiers = body["jobIdentifiers"].as_array().ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "jobIdentifiers is required",
        )
    })?;

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    let mut errors: Vec<Value> = Vec::new();
    let mut deleted: Vec<Value> = Vec::new();

    for identifier in job_identifiers {
        let id = identifier.as_str().unwrap_or_default();
        let key = s
            .evaluation_jobs
            .iter()
            .find(|(k, j)| *k == id || j.job_name == id || j.job_arn.ends_with(&format!("/{id}")))
            .map(|(k, _)| k.clone());

        match key {
            Some(k) => {
                let job_arn = s
                    .evaluation_jobs
                    .get(&k)
                    .map(|j| j.job_arn.clone())
                    .unwrap_or_else(|| k.clone());
                s.evaluation_jobs.remove(&k);
                deleted.push(json!({
                    "jobIdentifier": job_arn,
                    "jobStatus": "Deleting",
                }));
            }
            None => {
                errors.push(json!({
                    "jobIdentifier": id,
                    "code": "JobNotFound",
                    "message": format!("Evaluation job {id} not found")
                }));
            }
        }
    }

    Ok(AwsResponse::ok_json(json!({
        "errors": errors,
        "evaluationJobs": deleted,
    })))
}

fn find_job<'a>(
    jobs: &'a std::collections::BTreeMap<String, EvaluationJob>,
    id_or_arn: &str,
) -> Result<&'a EvaluationJob, AwsServiceError> {
    jobs.get(id_or_arn)
        .or_else(|| {
            jobs.values()
                .find(|j| j.job_name == id_or_arn || j.job_arn.ends_with(&format!("/{id_or_arn}")))
        })
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Evaluation job {id_or_arn} not found"),
            )
        })
}

fn find_job_key(
    jobs: &std::collections::BTreeMap<String, EvaluationJob>,
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
                format!("Evaluation job {id_or_arn} not found"),
            )
        })
}

#[cfg(test)]
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
            action: "Eval".to_string(),
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

    #[test]
    fn create_evaluation_job_automated_detected_from_config() {
        let s = shared();
        let body = json!({
            "jobName": "auto-eval",
            "roleArn": "arn:aws:iam::123:role/bedrock",
            "evaluationConfig": {"automated": {"datasetMetricConfigs": []}}
        });
        let resp = create_evaluation_job(&s, &req(), &body).unwrap();
        assert_eq!(resp.status, StatusCode::CREATED);
        let state = s.read();
        assert_eq!(state.default_ref().evaluation_jobs.len(), 1);
        let job = state.default_ref().evaluation_jobs.values().next().unwrap();
        assert_eq!(job.job_type, "Automated");
        assert_eq!(job.job_name, "auto-eval");
        assert_eq!(job.status, "InProgress");
    }

    #[test]
    fn create_evaluation_job_defaults_to_human_without_automated() {
        let s = shared();
        let body = json!({"roleArn": "arn:aws:iam::123:role/b"});
        create_evaluation_job(&s, &req(), &body).unwrap();
        let state = s.read();
        let job = state.default_ref().evaluation_jobs.values().next().unwrap();
        assert_eq!(job.job_type, "Human");
        assert_eq!(job.job_name, "eval-job");
    }

    #[test]
    fn get_evaluation_job_by_arn_or_id_or_name() {
        let s = shared();
        let body = json!({"jobName": "my-eval"});
        create_evaluation_job(&s, &req(), &body).unwrap();
        let arn = s
            .read()
            .default_ref()
            .evaluation_jobs
            .keys()
            .next()
            .unwrap()
            .clone();
        let id = arn.rsplit('/').next().unwrap().to_string();
        assert!(get_evaluation_job(&s, &req(), &arn).is_ok());
        assert!(get_evaluation_job(&s, &req(), &id).is_ok());
        assert!(get_evaluation_job(&s, &req(), "my-eval").is_ok());
    }

    #[test]
    fn get_evaluation_job_unknown_returns_not_found() {
        let s = shared();
        let err = get_evaluation_job(&s, &req(), "missing").err().unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_evaluation_jobs_paginates() {
        let s = shared();
        for i in 0..3 {
            create_evaluation_job(&s, &req(), &json!({"jobName": format!("j{i}")})).unwrap();
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_evaluation_jobs(&s, &r).unwrap();
        let text = std::str::from_utf8(resp.body.expect_bytes()).unwrap();
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["jobSummaries"].as_array().unwrap().len(), 2);
        assert!(v["nextToken"].is_string());
    }

    #[test]
    fn stop_evaluation_job_transitions_to_stopped() {
        let s = shared();
        create_evaluation_job(&s, &req(), &json!({})).unwrap();
        let arn = s
            .read()
            .default_ref()
            .evaluation_jobs
            .keys()
            .next()
            .unwrap()
            .clone();
        stop_evaluation_job(&s, &req(), &arn).unwrap();
        assert_eq!(
            s.read().default_ref().evaluation_jobs[&arn].status,
            "Stopped"
        );
    }

    #[test]
    fn stop_evaluation_job_conflict_when_not_in_progress() {
        let s = shared();
        create_evaluation_job(&s, &req(), &json!({})).unwrap();
        let arn = s
            .read()
            .default_ref()
            .evaluation_jobs
            .keys()
            .next()
            .unwrap()
            .clone();
        stop_evaluation_job(&s, &req(), &arn).unwrap();
        let err = stop_evaluation_job(&s, &req(), &arn).err().unwrap();
        assert_eq!(err.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn stop_evaluation_job_unknown_returns_not_found() {
        let s = shared();
        let err = stop_evaluation_job(&s, &req(), "missing").err().unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn batch_delete_removes_matches_and_collects_errors() {
        let s = shared();
        create_evaluation_job(&s, &req(), &json!({"jobName": "a"})).unwrap();
        create_evaluation_job(&s, &req(), &json!({"jobName": "b"})).unwrap();
        let body = json!({"jobIdentifiers": ["a", "missing"]});
        let resp = batch_delete_evaluation_job(&s, &req(), &body).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        let errors = v["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["jobIdentifier"], "missing");
        assert_eq!(s.read().default_ref().evaluation_jobs.len(), 1);
    }

    #[test]
    fn batch_delete_missing_identifiers_returns_validation_error() {
        let s = shared();
        let err = batch_delete_evaluation_job(&s, &req(), &json!({}))
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }
}
