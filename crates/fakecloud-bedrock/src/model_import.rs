use chrono::Utc;
use http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::{ImportedModel, ModelImportJob, SharedBedrockState};

pub(crate) fn create_model_import_job(
    state: &SharedBedrockState,
    req: &AwsRequest,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let job_name = body["jobName"].as_str().unwrap_or("import-job");
    let imported_model_name = body["importedModelName"].as_str().unwrap_or(job_name);
    let role_arn = body["roleArn"].as_str().unwrap_or_default();

    let job_id = Uuid::new_v4().to_string();
    let job_arn = crate::arns::model_import_job_arn(&req.region, &req.account_id, &job_id);
    let imported_model_arn =
        crate::arns::imported_model_arn(&req.region, &req.account_id, imported_model_name);

    let now = Utc::now();
    let model_data_source = body.get("modelDataSource").cloned().unwrap_or(json!({}));

    let job = ModelImportJob {
        job_arn: job_arn.clone(),
        job_name: job_name.to_string(),
        imported_model_name: imported_model_name.to_string(),
        imported_model_arn: imported_model_arn.clone(),
        role_arn: role_arn.to_string(),
        model_data_source: model_data_source.clone(),
        status: "Completed".to_string(),
        creation_time: now,
        last_modified_time: now,
    };

    let imported_model = ImportedModel {
        model_arn: imported_model_arn.clone(),
        model_name: imported_model_name.to_string(),
        job_arn: job_arn.clone(),
        model_data_source,
        creation_time: now,
    };

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    s.model_import_jobs.insert(job_arn.clone(), job);
    s.imported_models
        .insert(imported_model_arn.clone(), imported_model);

    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({ "jobArn": job_arn }),
    ))
}

pub(crate) fn get_model_import_job(
    state: &SharedBedrockState,
    req: &AwsRequest,
    job_identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let job = s
        .model_import_jobs
        .get(job_identifier)
        .or_else(|| {
            s.model_import_jobs.values().find(|j| {
                j.job_name == job_identifier || j.job_arn.ends_with(&format!("/{job_identifier}"))
            })
        })
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Model import job {job_identifier} not found"),
            )
        })?;

    Ok(AwsResponse::ok_json(json!({
        "jobArn": job.job_arn,
        "jobName": job.job_name,
        "importedModelName": job.imported_model_name,
        "importedModelArn": job.imported_model_arn,
        "roleArn": job.role_arn,
        "modelDataSource": job.model_data_source,
        "status": job.status,
        "creationTime": job.creation_time.to_rfc3339(),
        "lastModifiedTime": job.last_modified_time.to_rfc3339(),
    })))
}

pub(crate) fn list_model_import_jobs(
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
    let mut items: Vec<&ModelImportJob> = s.model_import_jobs.values().collect();
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
                "importedModelName": j.imported_model_name,
                "importedModelArn": j.imported_model_arn,
                "creationTime": j.creation_time.to_rfc3339(),
                "lastModifiedTime": j.last_modified_time.to_rfc3339(),
            })
        })
        .collect();

    let mut resp = json!({ "modelImportJobSummaries": page });
    let end = start.saturating_add(max_results);
    if end < items.len() {
        if let Some(last) = items.get(end - 1) {
            resp["nextToken"] = json!(last.job_arn);
        }
    }

    Ok(AwsResponse::ok_json(resp))
}

pub(crate) fn get_imported_model(
    state: &SharedBedrockState,
    req: &AwsRequest,
    model_identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let model = s
        .imported_models
        .get(model_identifier)
        .or_else(|| {
            s.imported_models.values().find(|m| {
                m.model_name == model_identifier
                    || m.model_arn.ends_with(&format!("/{model_identifier}"))
            })
        })
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Imported model {model_identifier} not found"),
            )
        })?;

    Ok(AwsResponse::ok_json(json!({
        "modelArn": model.model_arn,
        "modelName": model.model_name,
        "jobArn": model.job_arn,
        "modelDataSource": model.model_data_source,
        "creationTime": model.creation_time.to_rfc3339(),
    })))
}

pub(crate) fn list_imported_models(
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
    let mut items: Vec<&ImportedModel> = s.imported_models.values().collect();
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

pub(crate) fn delete_imported_model(
    state: &SharedBedrockState,
    req: &AwsRequest,
    model_identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let Some(s) = accts.get_mut(&req.account_id) else {
        return Err(AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Imported model {model_identifier} not found"),
        ));
    };
    let key = s
        .imported_models
        .iter()
        .find(|(k, m)| {
            *k == model_identifier
                || m.model_name == model_identifier
                || m.model_arn.ends_with(&format!("/{model_identifier}"))
        })
        .map(|(k, _)| k.clone());

    match key {
        Some(k) => {
            s.imported_models.remove(&k);
            Ok(AwsResponse::json(StatusCode::OK, "{}".to_string()))
        }
        None => Err(AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Imported model {model_identifier} not found"),
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

    fn create_import(state: &SharedBedrockState, job_name: &str) -> (String, String) {
        let resp = create_model_import_job(
            state,
            &req(),
            &json!({"jobName": job_name, "importedModelName": job_name, "roleArn": "role"}),
        )
        .unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        let job_arn = v["jobArn"].as_str().unwrap().to_string();
        let accts = state.read();
        let s = accts.default_ref();
        let model_arn = s.model_import_jobs[&job_arn].imported_model_arn.clone();
        (job_arn, model_arn)
    }

    #[test]
    fn create_import_defaults_name_and_role() {
        let s = shared();
        let resp = create_model_import_job(&s, &req(), &json!({})).unwrap();
        assert_eq!(resp.status, StatusCode::CREATED);
        let state = s.read();
        let acct = state.default_ref();
        assert_eq!(acct.model_import_jobs.len(), 1);
        assert_eq!(acct.imported_models.len(), 1);
    }

    #[test]
    fn get_import_job_by_arn_or_name_or_id() {
        let s = shared();
        let (arn, _) = create_import(&s, "my-import");
        let id = arn.rsplit('/').next().unwrap().to_string();
        assert!(get_model_import_job(&s, &req(), &arn).is_ok());
        assert!(get_model_import_job(&s, &req(), &id).is_ok());
        assert!(get_model_import_job(&s, &req(), "my-import").is_ok());
    }

    #[test]
    fn get_import_job_unknown_not_found() {
        let s = shared();
        let err = get_model_import_job(&s, &req(), "missing").err().unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_import_jobs_paginates() {
        let s = shared();
        for i in 0..3 {
            create_import(&s, &format!("j{i}"));
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_model_import_jobs(&s, &r).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["modelImportJobSummaries"].as_array().unwrap().len(), 2);
        assert!(v["nextToken"].is_string());
    }

    #[test]
    fn get_imported_model_by_arn_or_name_or_id() {
        let s = shared();
        let (_, model_arn) = create_import(&s, "my-model");
        let id = model_arn.rsplit('/').next().unwrap().to_string();
        assert!(get_imported_model(&s, &req(), &model_arn).is_ok());
        assert!(get_imported_model(&s, &req(), &id).is_ok());
        assert!(get_imported_model(&s, &req(), "my-model").is_ok());
    }

    #[test]
    fn get_imported_model_unknown_not_found() {
        let s = shared();
        let err = get_imported_model(&s, &req(), "missing").err().unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_imported_models_paginates() {
        let s = shared();
        for i in 0..3 {
            create_import(&s, &format!("m{i}"));
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_imported_models(&s, &r).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["modelSummaries"].as_array().unwrap().len(), 2);
        assert!(v["nextToken"].is_string());
    }

    #[test]
    fn delete_imported_model_removes_entry() {
        let s = shared();
        let (_, model_arn) = create_import(&s, "del");
        delete_imported_model(&s, &req(), &model_arn).unwrap();
        assert!(s.read().default_ref().imported_models.is_empty());
    }

    #[test]
    fn delete_imported_model_unknown_not_found() {
        let s = shared();
        let err = delete_imported_model(&s, &req(), "missing").err().unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn delete_imported_model_no_account_not_found() {
        let s = shared();
        let mut r = req();
        r.account_id = "999999999999".to_string();
        let err = delete_imported_model(&s, &r, "any").err().unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }
}
