use chrono::Utc;
use http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use fakecloud_core::service::{AwsRequest, AwsResponse, AwsServiceError};

use crate::state::{AutomatedReasoningPolicy, AutomatedReasoningTestCase, SharedBedrockState};

// Policy CRUD

pub(crate) fn create_automated_reasoning_policy(
    state: &SharedBedrockState,
    req: &AwsRequest,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let policy_name = body["policyName"].as_str().ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "policyName is required",
        )
    })?;

    let policy_id = Uuid::new_v4().to_string();
    let policy_arn =
        crate::arns::automated_reasoning_policy_arn(&req.region, &req.account_id, &policy_id);

    let now = Utc::now();
    let policy = AutomatedReasoningPolicy {
        policy_arn: policy_arn.clone(),
        policy_name: policy_name.to_string(),
        description: body["description"].as_str().map(String::from),
        policy_document: body.get("policyDocument").cloned().unwrap_or(json!({})),
        status: "ACTIVE".to_string(),
        version: "1".to_string(),
        versions: vec!["1".to_string()],
        created_at: now,
        updated_at: now,
    };

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);
    s.automated_reasoning_policies
        .insert(policy_arn.clone(), policy);

    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({ "policyArn": policy_arn }),
    ))
}

pub(crate) fn get_automated_reasoning_policy(
    state: &SharedBedrockState,
    req: &AwsRequest,
    identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let policy = find_policy(&s.automated_reasoning_policies, identifier).ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Automated reasoning policy {identifier} not found"),
        )
    })?;

    Ok(AwsResponse::ok_json(policy_to_json(policy)))
}

pub(crate) fn list_automated_reasoning_policies(
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
    let mut items: Vec<&AutomatedReasoningPolicy> =
        s.automated_reasoning_policies.values().collect();
    items.sort_by(|a, b| a.policy_arn.cmp(&b.policy_arn));

    let start = if let Some(token) = next_token {
        items
            .iter()
            .position(|p| p.policy_arn.as_str() > token.as_str())
            .unwrap_or(items.len())
    } else {
        0
    };

    let page: Vec<Value> = items
        .iter()
        .skip(start)
        .take(max_results)
        .map(|p| {
            // The Smithy summary requires `policyArn`, `name`, `policyId`,
            // `version`, `createdAt`, and `updatedAt`. `policyId` is the trailing
            // path segment of the ARN.
            json!({
                "policyArn": p.policy_arn,
                "name": p.policy_name,
                "policyId": policy_id_from_arn(&p.policy_arn),
                "description": p.description,
                "version": p.version,
                "createdAt": p.created_at.to_rfc3339(),
                "updatedAt": p.updated_at.to_rfc3339(),
            })
        })
        .collect();

    let mut resp = json!({ "automatedReasoningPolicySummaries": page });
    let end = start.saturating_add(max_results);
    if end < items.len() {
        if let Some(last) = items.get(end - 1) {
            resp["nextToken"] = json!(last.policy_arn);
        }
    }

    Ok(AwsResponse::ok_json(resp))
}

pub(crate) fn update_automated_reasoning_policy(
    state: &SharedBedrockState,
    req: &AwsRequest,
    identifier: &str,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let key = find_policy_key(&s.automated_reasoning_policies, identifier).ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Automated reasoning policy {identifier} not found"),
        )
    })?;

    // SAFETY: key was just validated by find_policy_key above while holding the write lock
    let policy = s
        .automated_reasoning_policies
        .get_mut(&key)
        .expect("key validated by find_policy_key");

    if let Some(name) = body["policyName"].as_str() {
        policy.policy_name = name.to_string();
    }
    if let Some(desc) = body.get("description") {
        policy.description = desc.as_str().map(String::from);
    }
    if let Some(doc) = body.get("policyDocument") {
        policy.policy_document = doc.clone();
    }
    policy.updated_at = Utc::now();

    Ok(AwsResponse::ok_json(policy_to_json(policy)))
}

pub(crate) fn delete_automated_reasoning_policy(
    state: &SharedBedrockState,
    req: &AwsRequest,
    identifier: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let key = find_policy_key(&s.automated_reasoning_policies, identifier).ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Automated reasoning policy {identifier} not found"),
        )
    })?;

    // Remove associated test cases
    let policy_arn = key.clone();
    s.automated_reasoning_test_cases
        .retain(|(arn, _), _| *arn != policy_arn);

    s.automated_reasoning_policies.remove(&key);
    Ok(AwsResponse::json(StatusCode::OK, "{}".to_string()))
}

// Policy versions

pub(crate) fn create_automated_reasoning_policy_version(
    state: &SharedBedrockState,
    req: &AwsRequest,
    identifier: &str,
    _body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let key = find_policy_key(&s.automated_reasoning_policies, identifier).ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Automated reasoning policy {identifier} not found"),
        )
    })?;

    let policy = s
        .automated_reasoning_policies
        .get_mut(&key)
        .expect("key validated by find_policy_key");

    let current: u32 = policy.version.parse().unwrap_or(1);
    let next = current.saturating_add(1);
    let version_str = next.to_string();
    policy.version = version_str.clone();
    policy.versions.push(version_str.clone());
    policy.updated_at = Utc::now();

    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({
            "policyArn": policy.policy_arn,
            "version": version_str,
        }),
    ))
}

pub(crate) fn export_automated_reasoning_policy_version(
    state: &SharedBedrockState,
    identifier: &str,
    req: &AwsRequest,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);
    let policy = find_policy(&s.automated_reasoning_policies, identifier).ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Automated reasoning policy {identifier} not found"),
        )
    })?;

    let requested_version = req.query_params.get("policyVersion");
    if let Some(ver) = requested_version {
        if !policy.versions.contains(ver) {
            return Err(AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Version {ver} not found for policy {identifier}"),
            ));
        }
    }

    Ok(AwsResponse::ok_json(json!({
        "policyArn": policy.policy_arn,
        "policyDocument": policy.policy_document,
        "version": requested_version.unwrap_or(&policy.version),
    })))
}

// Test cases

pub(crate) fn create_automated_reasoning_policy_test_case(
    state: &SharedBedrockState,
    req: &AwsRequest,
    policy_identifier: &str,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let test_case_name = body["testCaseName"].as_str().ok_or_else(|| {
        AwsServiceError::aws_error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "testCaseName is required",
        )
    })?;

    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let policy_arn = find_policy_key(&s.automated_reasoning_policies, policy_identifier)
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Automated reasoning policy {policy_identifier} not found"),
            )
        })?;

    let test_case_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let tc = AutomatedReasoningTestCase {
        test_case_id: test_case_id.clone(),
        policy_arn: policy_arn.clone(),
        test_case_name: test_case_name.to_string(),
        description: body["description"].as_str().map(String::from),
        input: body.get("input").cloned().unwrap_or(json!({})),
        expected_output: body.get("expectedOutput").cloned().unwrap_or(json!({})),
        created_at: now,
        updated_at: now,
    };

    s.automated_reasoning_test_cases
        .insert((policy_arn, test_case_id.clone()), tc);

    Ok(AwsResponse::json_value(
        StatusCode::CREATED,
        json!({ "testCaseId": test_case_id }),
    ))
}

pub(crate) fn get_automated_reasoning_policy_test_case(
    state: &SharedBedrockState,
    req: &AwsRequest,
    policy_identifier: &str,
    test_case_id: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let accts = state.read();
    let empty = crate::state::BedrockState::new(&req.account_id, &req.region);
    let s = accts.get(&req.account_id).unwrap_or(&empty);

    let policy_arn = find_policy_key(&s.automated_reasoning_policies, policy_identifier)
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Automated reasoning policy {policy_identifier} not found"),
            )
        })?;

    let tc = s
        .automated_reasoning_test_cases
        .get(&(policy_arn, test_case_id.to_string()))
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Test case {test_case_id} not found"),
            )
        })?;

    Ok(AwsResponse::ok_json(test_case_to_json(tc)))
}

pub(crate) fn list_automated_reasoning_policy_test_cases(
    state: &SharedBedrockState,
    policy_identifier: &str,
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

    let policy_arn = find_policy_key(&s.automated_reasoning_policies, policy_identifier)
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Automated reasoning policy {policy_identifier} not found"),
            )
        })?;

    let mut items: Vec<&AutomatedReasoningTestCase> = s
        .automated_reasoning_test_cases
        .iter()
        .filter(|((arn, _), _)| *arn == policy_arn)
        .map(|(_, tc)| tc)
        .collect();
    items.sort_by(|a, b| a.test_case_id.cmp(&b.test_case_id));

    let start = if let Some(token) = next_token {
        items
            .iter()
            .position(|tc| tc.test_case_id.as_str() > token.as_str())
            .unwrap_or(items.len())
    } else {
        0
    };

    let page: Vec<Value> = items
        .iter()
        .skip(start)
        .take(max_results)
        .map(|tc| {
            json!({
                "testCaseId": tc.test_case_id,
                "testCaseName": tc.test_case_name,
                "description": tc.description,
                "createdAt": tc.created_at.to_rfc3339(),
                "updatedAt": tc.updated_at.to_rfc3339(),
            })
        })
        .collect();

    let mut resp = json!({ "testCaseSummaries": page });
    let end = start.saturating_add(max_results);
    if end < items.len() {
        if let Some(last) = items.get(end - 1) {
            resp["nextToken"] = json!(last.test_case_id);
        }
    }

    Ok(AwsResponse::ok_json(resp))
}

pub(crate) fn update_automated_reasoning_policy_test_case(
    state: &SharedBedrockState,
    req: &AwsRequest,
    policy_identifier: &str,
    test_case_id: &str,
    body: &Value,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let policy_arn = find_policy_key(&s.automated_reasoning_policies, policy_identifier)
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Automated reasoning policy {policy_identifier} not found"),
            )
        })?;

    let tc = s
        .automated_reasoning_test_cases
        .get_mut(&(policy_arn, test_case_id.to_string()))
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Test case {test_case_id} not found"),
            )
        })?;

    if let Some(name) = body["testCaseName"].as_str() {
        tc.test_case_name = name.to_string();
    }
    if let Some(desc) = body.get("description") {
        tc.description = desc.as_str().map(String::from);
    }
    if let Some(input) = body.get("input") {
        tc.input = input.clone();
    }
    if let Some(output) = body.get("expectedOutput") {
        tc.expected_output = output.clone();
    }
    tc.updated_at = Utc::now();

    Ok(AwsResponse::ok_json(test_case_to_json(tc)))
}

pub(crate) fn delete_automated_reasoning_policy_test_case(
    state: &SharedBedrockState,
    req: &AwsRequest,
    policy_identifier: &str,
    test_case_id: &str,
) -> Result<AwsResponse, AwsServiceError> {
    let mut accts = state.write();
    let s = accts.get_or_create(&req.account_id);

    let policy_arn = find_policy_key(&s.automated_reasoning_policies, policy_identifier)
        .ok_or_else(|| {
            AwsServiceError::aws_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFoundException",
                format!("Automated reasoning policy {policy_identifier} not found"),
            )
        })?;

    let key = (policy_arn, test_case_id.to_string());
    if s.automated_reasoning_test_cases.remove(&key).is_none() {
        return Err(AwsServiceError::aws_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFoundException",
            format!("Test case {test_case_id} not found"),
        ));
    }

    Ok(AwsResponse::json(StatusCode::OK, "{}".to_string()))
}

// Helpers

fn find_policy<'a>(
    policies: &'a std::collections::BTreeMap<String, AutomatedReasoningPolicy>,
    identifier: &str,
) -> Option<&'a AutomatedReasoningPolicy> {
    policies.get(identifier).or_else(|| {
        policies.values().find(|p| {
            p.policy_name == identifier || p.policy_arn.ends_with(&format!("/{identifier}"))
        })
    })
}

fn find_policy_key(
    policies: &std::collections::BTreeMap<String, AutomatedReasoningPolicy>,
    identifier: &str,
) -> Option<String> {
    policies
        .iter()
        .find(|(k, p)| {
            *k == identifier
                || p.policy_name == identifier
                || p.policy_arn.ends_with(&format!("/{identifier}"))
        })
        .map(|(k, _)| k.clone())
}

/// Trailing segment of the automated-reasoning policy ARN. The Smithy summary
/// requires this as a separate `policyId` field even though the ARN already
/// contains it.
fn policy_id_from_arn(arn: &str) -> String {
    arn.rsplit('/').next().unwrap_or(arn).to_string()
}

fn policy_to_json(p: &AutomatedReasoningPolicy) -> Value {
    json!({
        "policyArn": p.policy_arn,
        "policyName": p.policy_name,
        "description": p.description,
        "policyDocument": p.policy_document,
        "status": p.status,
        "version": p.version,
        "versions": p.versions,
        "createdAt": p.created_at.to_rfc3339(),
        "updatedAt": p.updated_at.to_rfc3339(),
    })
}

fn test_case_to_json(tc: &AutomatedReasoningTestCase) -> Value {
    json!({
        "testCaseId": tc.test_case_id,
        "policyArn": tc.policy_arn,
        "testCaseName": tc.test_case_name,
        "description": tc.description,
        "input": tc.input,
        "expectedOutput": tc.expected_output,
        "createdAt": tc.created_at.to_rfc3339(),
        "updatedAt": tc.updated_at.to_rfc3339(),
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

    fn create_policy(state: &SharedBedrockState, name: &str) -> String {
        let resp = create_automated_reasoning_policy(
            state,
            &req(),
            &json!({"policyName": name, "policyDocument": {"doc": 1}}),
        )
        .unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        v["policyArn"].as_str().unwrap().to_string()
    }

    #[test]
    fn create_policy_missing_name_errors() {
        let s = shared();
        let err = create_automated_reasoning_policy(&s, &req(), &json!({}))
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_policy_persists() {
        let s = shared();
        create_policy(&s, "pol-1");
        assert_eq!(s.read().default_ref().automated_reasoning_policies.len(), 1);
    }

    #[test]
    fn get_policy_by_name_and_id_and_arn() {
        let s = shared();
        let arn = create_policy(&s, "pol-lookup");
        let id = arn.rsplit('/').next().unwrap().to_string();
        assert!(get_automated_reasoning_policy(&s, &req(), &arn).is_ok());
        assert!(get_automated_reasoning_policy(&s, &req(), &id).is_ok());
        assert!(get_automated_reasoning_policy(&s, &req(), "pol-lookup").is_ok());
    }

    #[test]
    fn get_policy_unknown_not_found() {
        let s = shared();
        let err = get_automated_reasoning_policy(&s, &req(), "missing")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_policies_paginates() {
        let s = shared();
        for i in 0..3 {
            create_policy(&s, &format!("pol-{i}"));
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_automated_reasoning_policies(&s, &r).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(
            v["automatedReasoningPolicySummaries"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(v["nextToken"].is_string());
    }

    #[test]
    fn update_policy_changes_fields() {
        let s = shared();
        let arn = create_policy(&s, "upd");
        update_automated_reasoning_policy(
            &s,
            &req(),
            &arn,
            &json!({"policyName": "new-name", "description": "desc"}),
        )
        .unwrap();
        let guard = s.read();
        let p = &guard.default_ref().automated_reasoning_policies[&arn];
        assert_eq!(p.policy_name, "new-name");
        assert_eq!(p.description.as_deref(), Some("desc"));
    }

    #[test]
    fn update_policy_unknown_not_found() {
        let s = shared();
        let err = update_automated_reasoning_policy(&s, &req(), "miss", &json!({}))
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn delete_policy_removes_test_cases() {
        let s = shared();
        let arn = create_policy(&s, "del");
        create_automated_reasoning_policy_test_case(
            &s,
            &req(),
            &arn,
            &json!({"testCaseName": "tc"}),
        )
        .unwrap();
        delete_automated_reasoning_policy(&s, &req(), &arn).unwrap();
        assert!(s
            .read()
            .default_ref()
            .automated_reasoning_policies
            .is_empty());
        assert!(s
            .read()
            .default_ref()
            .automated_reasoning_test_cases
            .is_empty());
    }

    #[test]
    fn delete_policy_unknown_not_found() {
        let s = shared();
        let err = delete_automated_reasoning_policy(&s, &req(), "miss")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn create_version_bumps_and_appends() {
        let s = shared();
        let arn = create_policy(&s, "ver");
        create_automated_reasoning_policy_version(&s, &req(), &arn, &json!({})).unwrap();
        let guard = s.read();
        let p = &guard.default_ref().automated_reasoning_policies[&arn];
        assert_eq!(p.version, "2");
        assert_eq!(p.versions, vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn create_version_unknown_not_found() {
        let s = shared();
        let err = create_automated_reasoning_policy_version(&s, &req(), "miss", &json!({}))
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn export_version_matches_stored_doc() {
        let s = shared();
        let arn = create_policy(&s, "exp");
        let resp = export_automated_reasoning_policy_version(&s, &arn, &req()).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["policyArn"], arn);
        assert_eq!(v["policyDocument"]["doc"], 1);
    }

    #[test]
    fn export_version_unknown_version_not_found() {
        let s = shared();
        let arn = create_policy(&s, "exp2");
        let mut r = req();
        r.query_params
            .insert("policyVersion".to_string(), "99".to_string());
        let err = export_automated_reasoning_policy_version(&s, &arn, &r)
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_case_crud_roundtrip() {
        let s = shared();
        let arn = create_policy(&s, "tc-host");
        let resp = create_automated_reasoning_policy_test_case(
            &s,
            &req(),
            &arn,
            &json!({"testCaseName": "tc1", "input": {"q": 1}, "expectedOutput": {"r": 2}}),
        )
        .unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        let id = v["testCaseId"].as_str().unwrap().to_string();

        let resp = get_automated_reasoning_policy_test_case(&s, &req(), &arn, &id).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["testCaseName"], "tc1");

        update_automated_reasoning_policy_test_case(
            &s,
            &req(),
            &arn,
            &id,
            &json!({"testCaseName": "tc1-new", "description": "d"}),
        )
        .unwrap();
        let updated_name = s.read().default_ref().automated_reasoning_test_cases
            [&(arn.clone(), id.clone())]
            .test_case_name
            .clone();
        assert_eq!(updated_name, "tc1-new");

        delete_automated_reasoning_policy_test_case(&s, &req(), &arn, &id).unwrap();
        assert!(s
            .read()
            .default_ref()
            .automated_reasoning_test_cases
            .is_empty());
    }

    #[test]
    fn create_test_case_missing_name_errors() {
        let s = shared();
        let arn = create_policy(&s, "p");
        let err = create_automated_reasoning_policy_test_case(&s, &req(), &arn, &json!({}))
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_test_case_unknown_policy_not_found() {
        let s = shared();
        let err = create_automated_reasoning_policy_test_case(
            &s,
            &req(),
            "missing-policy",
            &json!({"testCaseName": "tc"}),
        )
        .err()
        .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn get_test_case_unknown_policy_not_found() {
        let s = shared();
        let err = get_automated_reasoning_policy_test_case(&s, &req(), "miss", "id")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn get_test_case_unknown_id_not_found() {
        let s = shared();
        let arn = create_policy(&s, "p");
        let err = get_automated_reasoning_policy_test_case(&s, &req(), &arn, "missing")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_test_cases_paginates() {
        let s = shared();
        let arn = create_policy(&s, "pol");
        for i in 0..3 {
            create_automated_reasoning_policy_test_case(
                &s,
                &req(),
                &arn,
                &json!({"testCaseName": format!("tc-{i}")}),
            )
            .unwrap();
        }
        let mut r = req();
        r.query_params
            .insert("maxResults".to_string(), "2".to_string());
        let resp = list_automated_reasoning_policy_test_cases(&s, &arn, &r).unwrap();
        let v: Value =
            serde_json::from_str(std::str::from_utf8(resp.body.expect_bytes()).unwrap()).unwrap();
        assert_eq!(v["testCaseSummaries"].as_array().unwrap().len(), 2);
        assert!(v["nextToken"].is_string());
    }

    #[test]
    fn list_test_cases_unknown_policy_not_found() {
        let s = shared();
        let err = list_automated_reasoning_policy_test_cases(&s, "miss", &req())
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn update_test_case_unknown_policy_not_found() {
        let s = shared();
        let err = update_automated_reasoning_policy_test_case(&s, &req(), "miss", "id", &json!({}))
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn update_test_case_unknown_id_not_found() {
        let s = shared();
        let arn = create_policy(&s, "p2");
        let err =
            update_automated_reasoning_policy_test_case(&s, &req(), &arn, "missing", &json!({}))
                .err()
                .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn delete_test_case_unknown_policy_not_found() {
        let s = shared();
        let err = delete_automated_reasoning_policy_test_case(&s, &req(), "miss", "id")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn delete_test_case_unknown_id_not_found() {
        let s = shared();
        let arn = create_policy(&s, "p3");
        let err = delete_automated_reasoning_policy_test_case(&s, &req(), &arn, "missing")
            .err()
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
    }
}
