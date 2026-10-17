use super::*;
use bytes::Bytes;
use http::{HeaderMap, Method};
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

fn make_state() -> SharedBedrockState {
    Arc::new(RwLock::new(
        fakecloud_core::multi_account::MultiAccountState::new(
            "123456789012",
            "us-east-1",
            "http://localhost:4566",
        ),
    ))
}

fn make_request(method: Method, path: &str, body: &str) -> AwsRequest {
    let raw_path = path.to_string();
    let segs: Vec<String> = raw_path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    AwsRequest {
        service: "bedrock".to_string(),
        action: String::new(),
        region: "us-east-1".to_string(),
        account_id: "123456789012".to_string(),
        request_id: "test-id".to_string(),
        headers: HeaderMap::new(),
        query_params: HashMap::new(),
        body: Bytes::from(body.to_string()),
        body_stream: parking_lot::Mutex::new(None),
        path_segments: segs,
        raw_path,
        raw_query: String::new(),
        method,
        is_query_protocol: false,
        access_key_id: None,
        principal: None,
    }
}

fn make_request_with_query(
    method: Method,
    path: &str,
    body: &str,
    query: HashMap<String, String>,
) -> AwsRequest {
    let mut req = make_request(method, path, body);
    req.query_params = query;
    req
}

fn expect_err(result: Result<AwsResponse, AwsServiceError>) -> AwsServiceError {
    match result {
        Err(e) => e,
        Ok(_) => panic!("expected error, got Ok"),
    }
}

fn body_json(resp: &AwsResponse) -> Value {
    serde_json::from_slice(resp.body.expect_bytes()).unwrap()
}

// ── resolve_action routing ──

#[test]
fn resolve_action_list_foundation_models() {
    let req = make_request(Method::GET, "/foundation-models", "");
    let (action, id, _) = BedrockService::resolve_action(&req).unwrap();
    assert_eq!(action, "ListFoundationModels");
    assert!(id.is_none());
}

#[test]
fn resolve_action_trailing_slash_collection_lists() {
    // botocore < 1.40 / curl append `/` to a bare collection URI; it must
    // route to List, not GetFoundationModel("") (bug-audit 2026-06-20, 1.1).
    let req = make_request(Method::GET, "/foundation-models/", "");
    let (action, id, _) = BedrockService::resolve_action(&req).unwrap();
    assert_eq!(action, "ListFoundationModels");
    assert!(id.is_none());

    let req = make_request(Method::GET, "/guardrails/", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "ListGuardrails"
    );
}

#[test]
fn resolve_action_async_invoke_trailing_slash_still_validates() {
    // async-invoke deliberately routes a trailing slash to GetAsyncInvoke("")
    // so the length validator surfaces the missing identifier; the GET
    // empty-filtering must not regress that.
    let req = make_request(Method::GET, "/async-invoke", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "ListAsyncInvokes"
    );
    let req = make_request(Method::GET, "/async-invoke/", "");
    let (action, id, _) = BedrockService::resolve_action(&req).unwrap();
    assert_eq!(action, "GetAsyncInvoke");
    assert_eq!(id.unwrap(), "");
}

#[test]
fn resolve_action_get_foundation_model() {
    let req = make_request(
        Method::GET,
        "/foundation-models/anthropic.claude-3-5-sonnet-20241022-v2:0",
        "",
    );
    let (action, id, _) = BedrockService::resolve_action(&req).unwrap();
    assert_eq!(action, "GetFoundationModel");
    assert_eq!(id.unwrap(), "anthropic.claude-3-5-sonnet-20241022-v2:0");
}

#[test]
fn resolve_action_guardrail_crud() {
    let req = make_request(Method::POST, "/guardrails", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "CreateGuardrail"
    );

    let req = make_request(Method::GET, "/guardrails", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "ListGuardrails"
    );

    let req = make_request(Method::GET, "/guardrails/abc123", "");
    let (action, id, _) = BedrockService::resolve_action(&req).unwrap();
    assert_eq!(action, "GetGuardrail");
    assert_eq!(id.unwrap(), "abc123");

    let req = make_request(Method::PUT, "/guardrails/abc123", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "UpdateGuardrail"
    );

    let req = make_request(Method::DELETE, "/guardrails/abc123", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "DeleteGuardrail"
    );
}

#[test]
fn resolve_action_invoke_model() {
    let req = make_request(Method::POST, "/model/anthropic.claude-v2/invoke", "{}");
    let (action, id, _) = BedrockService::resolve_action(&req).unwrap();
    assert_eq!(action, "InvokeModel");
    assert_eq!(id.unwrap(), "anthropic.claude-v2");
}

#[test]
fn resolve_action_converse() {
    let req = make_request(Method::POST, "/model/anthropic.claude-v2/converse", "{}");
    let (action, id, _) = BedrockService::resolve_action(&req).unwrap();
    assert_eq!(action, "Converse");
    assert_eq!(id.unwrap(), "anthropic.claude-v2");
}

#[test]
fn resolve_action_converse_stream() {
    let req = make_request(
        Method::POST,
        "/model/anthropic.claude-v2/converse-stream",
        "{}",
    );
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "ConverseStream"
    );
}

#[test]
fn resolve_action_invoke_stream() {
    let req = make_request(Method::POST, "/model/m1/invoke-with-response-stream", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "InvokeModelWithResponseStream"
    );
}

#[test]
fn resolve_action_tags() {
    let req = make_request(Method::POST, "/tagResource", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "TagResource"
    );

    let req = make_request(Method::POST, "/untagResource", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "UntagResource"
    );

    let req = make_request(Method::POST, "/listTagsForResource", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "ListTagsForResource"
    );
}

#[test]
fn resolve_action_logging() {
    let req = make_request(Method::PUT, "/logging/modelinvocations", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "PutModelInvocationLoggingConfiguration"
    );

    let req = make_request(Method::GET, "/logging/modelinvocations", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "GetModelInvocationLoggingConfiguration"
    );

    let req = make_request(Method::DELETE, "/logging/modelinvocations", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "DeleteModelInvocationLoggingConfiguration"
    );
}

#[test]
fn resolve_action_custom_models() {
    let req = make_request(Method::POST, "/custom-models/create-custom-model", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "CreateCustomModel"
    );

    let req = make_request(Method::GET, "/custom-models", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "ListCustomModels"
    );

    let req = make_request(Method::GET, "/custom-models/my-model", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "GetCustomModel"
    );

    let req = make_request(Method::DELETE, "/custom-models/my-model", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "DeleteCustomModel"
    );
}

#[test]
fn resolve_action_inference_profiles() {
    let req = make_request(Method::POST, "/inference-profiles", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "CreateInferenceProfile"
    );

    let req = make_request(Method::GET, "/inference-profiles", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "ListInferenceProfiles"
    );

    let req = make_request(Method::GET, "/inference-profiles/ip-1", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "GetInferenceProfile"
    );

    let req = make_request(Method::DELETE, "/inference-profiles/ip-1", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "DeleteInferenceProfile"
    );
}

#[test]
fn resolve_action_provisioned_throughput() {
    let req = make_request(Method::POST, "/provisioned-model-throughput", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "CreateProvisionedModelThroughput"
    );

    let req = make_request(Method::GET, "/provisioned-model-throughputs", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "ListProvisionedModelThroughputs"
    );
}

#[test]
fn resolve_action_async_invoke() {
    let req = make_request(Method::POST, "/async-invoke", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "StartAsyncInvoke"
    );

    let req = make_request(Method::GET, "/async-invoke", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "ListAsyncInvokes"
    );

    let req = make_request(Method::GET, "/async-invoke/inv-1", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "GetAsyncInvoke"
    );
}

#[test]
fn resolve_action_unknown_returns_none() {
    let req = make_request(Method::POST, "/nonexistent", "{}");
    assert!(BedrockService::resolve_action(&req).is_none());
}

#[test]
fn resolve_action_apply_guardrail() {
    let req = make_request(Method::POST, "/guardrail/g1/version/1/apply", "{}");
    let (action, id, extra) = BedrockService::resolve_action(&req).unwrap();
    assert_eq!(action, "ApplyGuardrail");
    assert_eq!(id.unwrap(), "g1");
    assert_eq!(extra.unwrap(), "1");
}

#[test]
fn resolve_action_resource_policy() {
    let req = make_request(Method::POST, "/resource-policy", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "PutResourcePolicy"
    );

    let req = make_request(Method::GET, "/resource-policy/arn:aws:something", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "GetResourcePolicy"
    );

    let req = make_request(Method::DELETE, "/resource-policy/arn:aws:something", "");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "DeleteResourcePolicy"
    );
}

#[test]
fn resolve_action_count_tokens() {
    let req = make_request(Method::POST, "/model/m1/count-tokens", "{}");
    assert_eq!(
        BedrockService::resolve_action(&req).unwrap().0,
        "CountTokens"
    );
}

// ── ListFoundationModels ──

#[tokio::test]
async fn list_foundation_models_returns_models() {
    let state = make_state();
    let svc = BedrockService::new(state);
    let req = make_request(Method::GET, "/foundation-models", "");
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    let models = b["modelSummaries"].as_array().unwrap();
    assert!(!models.is_empty());
    // Each model has required fields
    let first = &models[0];
    assert!(first["modelId"].as_str().is_some());
    assert!(first["providerName"].as_str().is_some());
}

#[tokio::test]
async fn list_foundation_models_filter_by_provider() {
    let state = make_state();
    let svc = BedrockService::new(state);
    let mut query = HashMap::new();
    query.insert("byProvider".to_string(), "Anthropic".to_string());
    let req = make_request_with_query(Method::GET, "/foundation-models", "", query);
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    let models = b["modelSummaries"].as_array().unwrap();
    assert!(!models.is_empty());
    for m in models {
        assert_eq!(m["providerName"], "Anthropic");
    }
}

#[tokio::test]
async fn list_foundation_models_filter_by_output_modality() {
    let state = make_state();
    let svc = BedrockService::new(state);
    let mut query = HashMap::new();
    query.insert("byOutputModality".to_string(), "IMAGE".to_string());
    let req = make_request_with_query(Method::GET, "/foundation-models", "", query);
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    let models = b["modelSummaries"].as_array().unwrap();
    for m in models {
        let output = m["outputModalities"].as_array().unwrap();
        assert!(output.iter().any(|v| v == "IMAGE"));
    }
}

// ── GetFoundationModel ──

#[tokio::test]
async fn get_foundation_model_found() {
    let state = make_state();
    let svc = BedrockService::new(state);
    let req = make_request(
        Method::GET,
        "/foundation-models/anthropic.claude-3-5-sonnet-20241022-v2:0",
        "",
    );
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert_eq!(
        b["modelDetails"]["modelId"],
        "anthropic.claude-3-5-sonnet-20241022-v2:0"
    );
    assert!(b["modelDetails"]["modelArn"]
        .as_str()
        .unwrap()
        .contains("foundation-model"));
}

#[tokio::test]
async fn get_foundation_model_not_found() {
    let state = make_state();
    let svc = BedrockService::new(state);
    let req = make_request(Method::GET, "/foundation-models/nonexistent.model", "");
    let err = expect_err(svc.handle(req).await);
    assert!(err.to_string().contains("ResourceNotFoundException"));
}

// ── Tags ──

#[tokio::test]
async fn tag_list_untag_resource() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let arn = "arn:aws:bedrock:us-east-1:123456789012:guardrail/g1";
    let body = serde_json::json!({
        "resourceARN": arn,
        "tags": [{"key": "env", "value": "prod"}, {"key": "team", "value": "ml"}],
    });
    let req = make_request(Method::POST, "/tagResource", &body.to_string());
    svc.handle(req).await.unwrap();

    // List tags
    let body = serde_json::json!({"resourceARN": arn});
    let req = make_request(Method::POST, "/listTagsForResource", &body.to_string());
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    let tags = b["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2);

    // Untag
    let body = serde_json::json!({
        "resourceARN": arn,
        "tagKeys": ["env"],
    });
    let req = make_request(Method::POST, "/untagResource", &body.to_string());
    svc.handle(req).await.unwrap();

    // List again
    let body = serde_json::json!({"resourceARN": arn});
    let req = make_request(Method::POST, "/listTagsForResource", &body.to_string());
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    let tags = b["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0]["key"], "team");
}

#[tokio::test]
async fn tag_resource_requires_arn() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({
        "tags": [{"key": "a", "value": "b"}],
    });
    let req = make_request(Method::POST, "/tagResource", &body.to_string());
    let err = expect_err(svc.handle(req).await);
    assert!(err.to_string().contains("ValidationException"));
}

#[tokio::test]
async fn tag_resource_requires_tags() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({
        "resourceARN": "arn:aws:bedrock:us-east-1:123456789012:guardrail/g1",
    });
    let req = make_request(Method::POST, "/tagResource", &body.to_string());
    let err = expect_err(svc.handle(req).await);
    assert!(err.to_string().contains("ValidationException"));
}

// ── Guardrails ──

#[tokio::test]
async fn guardrail_crud() {
    let state = make_state();
    let svc = BedrockService::new(state);

    // Create
    let body = serde_json::json!({
        "name": "my-guardrail",
        "blockedInputMessaging": "Blocked input",
        "blockedOutputsMessaging": "Blocked output",
    });
    let req = make_request(Method::POST, "/guardrails", &body.to_string());
    let resp = svc.handle(req).await.unwrap();
    assert_eq!(resp.status, StatusCode::CREATED);
    let b = body_json(&resp);
    let gid = b["guardrailId"].as_str().unwrap().to_string();
    assert_eq!(b["version"], "DRAFT");

    // Get
    let req = make_request(Method::GET, &format!("/guardrails/{gid}"), "");
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert_eq!(b["name"], "my-guardrail");
    assert_eq!(b["status"], "READY");

    // List
    let req = make_request(Method::GET, "/guardrails", "");
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert_eq!(b["guardrails"].as_array().unwrap().len(), 1);

    // Update
    let body = serde_json::json!({
        "name": "updated-guardrail",
        "blockedInputMessaging": "New blocked",
        "blockedOutputsMessaging": "New blocked out",
    });
    let req = make_request(
        Method::PUT,
        &format!("/guardrails/{gid}"),
        &body.to_string(),
    );
    svc.handle(req).await.unwrap();

    // Verify update
    let req = make_request(Method::GET, &format!("/guardrails/{gid}"), "");
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert_eq!(b["name"], "updated-guardrail");

    // Delete
    let req = make_request(Method::DELETE, &format!("/guardrails/{gid}"), "");
    svc.handle(req).await.unwrap();

    // Get should fail
    let req = make_request(Method::GET, &format!("/guardrails/{gid}"), "");
    assert!(svc.handle(req).await.is_err());
}

#[tokio::test]
async fn guardrail_in_china_region_uses_aws_cn_partition() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({
        "name": "cn-guardrail",
        "blockedInputMessaging": "Blocked input",
        "blockedOutputsMessaging": "Blocked output",
    });
    let mut req = make_request(Method::POST, "/guardrails", &body.to_string());
    req.region = "cn-north-1".to_string();
    let b = body_json(&svc.handle(req).await.unwrap());
    let arn = b["guardrailArn"].as_str().unwrap().to_string();
    assert!(
        arn.starts_with("arn:aws-cn:bedrock:cn-north-1:123456789012:guardrail/"),
        "{arn}"
    );

    let encoded = arn.replace(':', "%3A").replace('/', "%2F");
    let mut req = make_request(Method::POST, &format!("/guardrails/{encoded}"), "{}");
    req.region = "cn-north-1".to_string();
    let b = body_json(&svc.handle(req).await.unwrap());
    assert_eq!(b["version"], "1");

    let mut req = make_request(Method::GET, "/foundation-models", "");
    req.region = "cn-north-1".to_string();
    let b = body_json(&svc.handle(req).await.unwrap());
    let model_arn = b["modelSummaries"][0]["modelArn"].as_str().unwrap();
    assert!(
        model_arn.starts_with("arn:aws-cn:bedrock:cn-north-1::foundation-model/"),
        "{model_arn}"
    );
}

#[tokio::test]
async fn guardrail_not_found() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let req = make_request(Method::GET, "/guardrails/nonexistent", "");
    let err = expect_err(svc.handle(req).await);
    assert!(err.to_string().contains("ResourceNotFoundException"));
}

// ── InvokeModel ──

#[tokio::test]
async fn invoke_model_anthropic() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({
        "messages": [{"role": "user", "content": "Hello"}],
    });
    let req = make_request(
        Method::POST,
        "/model/anthropic.claude-v2/invoke",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    assert_eq!(resp.status, StatusCode::OK);
    let b: Value = serde_json::from_slice(resp.body.expect_bytes()).unwrap();
    assert_eq!(b["type"], "message");
    assert_eq!(b["role"], "assistant");
}

#[tokio::test]
async fn invoke_model_amazon_titan() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({"inputText": "Hello"});
    let req = make_request(
        Method::POST,
        "/model/amazon.titan-text-express-v1/invoke",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    let b: Value = serde_json::from_slice(resp.body.expect_bytes()).unwrap();
    assert!(!b["results"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn invoke_model_meta_llama() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({"prompt": "Hello"});
    let req = make_request(
        Method::POST,
        "/model/meta.llama3-70b-instruct-v1:0/invoke",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    let b: Value = serde_json::from_slice(resp.body.expect_bytes()).unwrap();
    assert!(b["generation"].as_str().is_some());
}

#[tokio::test]
async fn invoke_model_cohere() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({"prompt": "Hello"});
    let req = make_request(
        Method::POST,
        "/model/cohere.command-r-v1:0/invoke",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    let b: Value = serde_json::from_slice(resp.body.expect_bytes()).unwrap();
    assert!(b["generations"].as_array().is_some());
}

#[tokio::test]
async fn invoke_model_mistral() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({"prompt": "Hello"});
    let req = make_request(
        Method::POST,
        "/model/mistral.mistral-7b-instruct-v0:2/invoke",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    let b: Value = serde_json::from_slice(resp.body.expect_bytes()).unwrap();
    assert!(b["outputs"].as_array().is_some());
}

#[tokio::test]
async fn invoke_model_records_invocation() {
    let state = make_state();
    let svc = BedrockService::new(state.clone());

    let body = serde_json::json!({"prompt": "test"});
    let req = make_request(
        Method::POST,
        "/model/anthropic.claude-v2/invoke",
        &body.to_string(),
    );
    svc.handle(req).await.unwrap();

    let _accts = state.read();
    let s = _accts.default_ref();
    assert_eq!(s.invocations.len(), 1);
    assert_eq!(s.invocations[0].model_id, "anthropic.claude-v2");
}

#[tokio::test]
async fn invoke_model_titan_embed() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({"inputText": "Hello"});
    let req = make_request(
        Method::POST,
        "/model/amazon.titan-embed-text-v1/invoke",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    let b: Value = serde_json::from_slice(resp.body.expect_bytes()).unwrap();
    assert!(b["embedding"].as_array().is_some());
}

// ── Converse ──

#[tokio::test]
async fn converse_basic() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "Hello world"}]}],
    });
    let req = make_request(
        Method::POST,
        "/model/anthropic.claude-v2/converse",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert_eq!(b["stopReason"], "end_turn");
    assert!(b["usage"]["inputTokens"].as_u64().is_some());
    assert!(b["output"]["message"]["content"][0]["text"]
        .as_str()
        .is_some());
}

#[tokio::test]
async fn converse_with_tool_config() {
    let state = make_state();
    let svc = BedrockService::new(state);

    // toolChoice `any` forces the model to call a tool.
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "Use the tool"}]}],
        "toolConfig": {
            "tools": [{"toolSpec": {"name": "calculator", "description": "calc"}}],
            "toolChoice": {"any": {}}
        },
    });
    let req = make_request(
        Method::POST,
        "/model/anthropic.claude-v2/converse",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert_eq!(b["stopReason"], "tool_use");
    let content = b["output"]["message"]["content"].as_array().unwrap();
    assert!(content.iter().any(|c| c.get("toolUse").is_some()));
}

#[tokio::test]
async fn converse_with_tool_config_without_choice_does_not_force_tool() {
    // A bare toolConfig (no toolChoice) must not push a spurious empty-arg
    // toolUse — that used to derail agent tool-loops.
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "toolConfig": {
            "tools": [{"toolSpec": {"name": "calculator", "description": "calc"}}]
        },
    });
    let req = make_request(
        Method::POST,
        "/model/anthropic.claude-v2/converse",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert_eq!(b["stopReason"], "end_turn");
    let content = b["output"]["message"]["content"].as_array().unwrap();
    assert!(content.iter().all(|c| c.get("toolUse").is_none()));
}

// ── CountTokens ──

#[tokio::test]
async fn count_tokens_basic() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({
        "input": {
            "converse": {
                "messages": [{"role": "user", "content": [{"text": "hello world foo bar"}]}]
            }
        }
    });
    let req = make_request(
        Method::POST,
        "/model/anthropic.claude-v2/count-tokens",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert!(b["inputTokens"].as_u64().unwrap() > 0);
}

// ── Logging ──

#[tokio::test]
async fn logging_configuration_crud() {
    let state = make_state();
    let svc = BedrockService::new(state);

    // Get before put -> empty
    let req = make_request(Method::GET, "/logging/modelinvocations", "");
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert!(b.get("loggingConfig").is_none());

    // Put
    let body = serde_json::json!({
        "loggingConfig": {
            "textDataDeliveryEnabled": true,
            "imageDataDeliveryEnabled": false,
            "s3Config": {"bucketName": "my-bucket", "keyPrefix": "logs/"},
        }
    });
    let req = make_request(Method::PUT, "/logging/modelinvocations", &body.to_string());
    svc.handle(req).await.unwrap();

    // Get after put
    let req = make_request(Method::GET, "/logging/modelinvocations", "");
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert_eq!(b["loggingConfig"]["textDataDeliveryEnabled"], true);
    assert_eq!(b["loggingConfig"]["imageDataDeliveryEnabled"], false);
    assert_eq!(b["loggingConfig"]["s3Config"]["bucketName"], "my-bucket");

    // Delete
    let req = make_request(Method::DELETE, "/logging/modelinvocations", "");
    svc.handle(req).await.unwrap();

    // Get after delete -> empty again
    let req = make_request(Method::GET, "/logging/modelinvocations", "");
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert!(b.get("loggingConfig").is_none());
}

// ── Resource policies ──

#[tokio::test]
async fn resource_policy_crud() {
    let state = make_state();
    let svc = BedrockService::new(state);

    // Use a simple ID for the path (no slashes) since path-based routing
    // splits on /. The ARN goes in the body for Put.
    let arn = "my-resource-arn";
    let policy = r#"{"Version":"2012-10-17","Statement":[]}"#;

    // Put
    let body = serde_json::json!({
        "resourceArn": arn,
        "resourcePolicy": policy,
    });
    let req = make_request(Method::POST, "/resource-policy", &body.to_string());
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert_eq!(b["resourceArn"], arn);

    // Get — path segment must be a single segment
    let req = make_request(Method::GET, &format!("/resource-policy/{arn}"), "");
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    assert_eq!(b["resourcePolicy"], policy);

    // Delete
    let req = make_request(Method::DELETE, &format!("/resource-policy/{arn}"), "");
    svc.handle(req).await.unwrap();

    // Get after delete -> not found
    let req = make_request(Method::GET, &format!("/resource-policy/{arn}"), "");
    assert!(svc.handle(req).await.is_err());
}

// ── Streaming ──

#[tokio::test]
async fn invoke_model_with_response_stream() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({"prompt": "Hello"});
    let req = make_request(
        Method::POST,
        "/model/anthropic.claude-v2/invoke-with-response-stream",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.content_type, "application/vnd.amazon.eventstream");
    assert!(!resp.body.expect_bytes().is_empty());
}

#[tokio::test]
async fn converse_stream() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "Hello"}]}],
    });
    let req = make_request(
        Method::POST,
        "/model/anthropic.claude-v2/converse-stream",
        &body.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    assert_eq!(resp.content_type, "application/vnd.amazon.eventstream");
    assert!(!resp.body.expect_bytes().is_empty());
}

#[tokio::test]
async fn converse_stream_matches_converse_for_forced_tool() {
    // Converse and ConverseStream must agree for identical input: a forced
    // toolChoice yields a tool call in both; a bare toolConfig yields neither.
    let state = make_state();
    let svc = BedrockService::new(state);

    let forced = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "toolConfig": {
            "tools": [{"toolSpec": {"name": "calculator"}}],
            "toolChoice": {"any": {}}
        },
    });
    let req = make_request(
        Method::POST,
        "/model/anthropic.claude-v2/converse-stream",
        &forced.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    let bytes = resp.body.expect_bytes();
    let s = String::from_utf8_lossy(bytes);
    assert!(
        s.contains("tool_use"),
        "forced stream should stop with tool_use"
    );
    assert!(
        s.contains("calculator"),
        "forced stream should name the tool"
    );

    // Bare toolConfig (no toolChoice): no tool, ends with end_turn.
    let bare = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "hi"}]}],
        "toolConfig": {
            "tools": [{"toolSpec": {"name": "calculator"}}]
        },
    });
    let req = make_request(
        Method::POST,
        "/model/anthropic.claude-v2/converse-stream",
        &bare.to_string(),
    );
    let resp = svc.handle(req).await.unwrap();
    let bytes = resp.body.expect_bytes();
    let s = String::from_utf8_lossy(bytes);
    assert!(
        s.contains("end_turn"),
        "bare stream should end with end_turn"
    );
    assert!(
        !s.contains("toolUse"),
        "bare stream must not emit a toolUse block"
    );
}

// ── Unknown route ──

#[tokio::test]
async fn unknown_route_returns_error() {
    let state = make_state();
    let svc = BedrockService::new(state);

    let req = make_request(Method::POST, "/nonexistent/route", "{}");
    assert!(svc.handle(req).await.is_err());
}

// ── Custom Models CRUD (direct handler calls to avoid ARN-in-path issues) ──

#[test]
fn custom_model_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/custom-models/create-custom-model",
        r#"{"modelName":"my-model"}"#,
    );
    let body = req.json_body();
    let resp = crate::custom_models::create_custom_model(&state, &req, &body).unwrap();
    assert_eq!(resp.status, StatusCode::CREATED);
    let b = body_json(&resp);
    let arn = b["modelArn"].as_str().unwrap();

    let resp = crate::custom_models::get_custom_model(&state, &req, arn).unwrap();
    let b = body_json(&resp);
    assert_eq!(b["modelName"], "my-model");

    let resp = crate::custom_models::list_custom_models(&state, &req).unwrap();
    let b = body_json(&resp);
    assert_eq!(b["modelSummaries"].as_array().unwrap().len(), 1);

    crate::custom_models::delete_custom_model(&state, &req, arn).unwrap();
    assert!(crate::custom_models::get_custom_model(&state, &req, arn).is_err());
}

#[test]
fn custom_model_deployment_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"modelDeploymentName":"dep1","modelArn":"m1"}"#,
    );
    let body = req.json_body();
    let resp = crate::custom_model_deployments::create_custom_model_deployment(&state, &req, &body)
        .unwrap();
    let b = body_json(&resp);
    let arn = b["customModelDeploymentArn"].as_str().unwrap();

    crate::custom_model_deployments::get_custom_model_deployment(&state, &req, arn).unwrap();

    let resp =
        crate::custom_model_deployments::list_custom_model_deployments(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["modelDeploymentSummaries"].as_array().unwrap().is_empty());

    let upd = serde_json::json!({"desiredModelUnits": 2});
    crate::custom_model_deployments::update_custom_model_deployment(&state, &req, arn, &upd)
        .unwrap();
    crate::custom_model_deployments::delete_custom_model_deployment(&state, &req, arn).unwrap();
}

#[test]
fn model_import_job_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"jobName":"imp","importedModelName":"m","roleArn":"arn:aws:iam::1:role/r","modelDataSource":{"s3DataSource":{"s3Uri":"s3://b"}}}"#,
    );
    let body = req.json_body();
    let resp = crate::model_import::create_model_import_job(&state, &req, &body).unwrap();
    let b = body_json(&resp);
    let arn = b["jobArn"].as_str().unwrap();

    crate::model_import::get_model_import_job(&state, &req, arn).unwrap();
    let resp = crate::model_import::list_model_import_jobs(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["modelImportJobSummaries"].as_array().unwrap().is_empty());

    let resp = crate::model_import::list_imported_models(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["modelSummaries"].as_array().unwrap().is_empty());
}

#[test]
fn model_copy_job_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"sourceModelArn":"arn:aws:bedrock:us-west-2:1:fm/m","targetModelName":"cp"}"#,
    );
    let body = req.json_body();
    let resp = crate::model_copy::create_model_copy_job(&state, &req, &body).unwrap();
    let b = body_json(&resp);
    let arn = b["jobArn"].as_str().unwrap();

    crate::model_copy::get_model_copy_job(&state, &req, arn).unwrap();
    let resp = crate::model_copy::list_model_copy_jobs(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["modelCopyJobSummaries"].as_array().unwrap().is_empty());
}

#[test]
fn invocation_job_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"jobName":"batch","modelId":"m","roleArn":"arn:aws:iam::1:role/r","inputDataConfig":{"s3InputDataConfig":{"s3Uri":"s3://i"}},"outputDataConfig":{"s3OutputDataConfig":{"s3Uri":"s3://o"}}}"#,
    );
    let body = req.json_body();
    let resp = crate::invocation_jobs::create_model_invocation_job(&state, &req, &body).unwrap();
    let b = body_json(&resp);
    let arn = b["jobArn"].as_str().unwrap();

    crate::invocation_jobs::get_model_invocation_job(&state, &req, arn).unwrap();
    let resp = crate::invocation_jobs::list_model_invocation_jobs(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["invocationJobSummaries"].as_array().unwrap().is_empty());
    crate::invocation_jobs::stop_model_invocation_job(&state, &req, arn).unwrap();
}

#[test]
fn evaluation_job_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"jobName":"eval","roleArn":"arn:aws:iam::1:role/r","evaluationConfig":{},"inferenceConfig":{},"outputDataConfig":{"s3Uri":"s3://o"}}"#,
    );
    let body = req.json_body();
    let resp = crate::evaluation::create_evaluation_job(&state, &req, &body).unwrap();
    let b = body_json(&resp);
    let arn = b["jobArn"].as_str().unwrap();

    crate::evaluation::get_evaluation_job(&state, &req, arn).unwrap();
    let resp = crate::evaluation::list_evaluation_jobs(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["jobSummaries"].as_array().unwrap().is_empty());
}

#[test]
fn inference_profile_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"inferenceProfileName":"prof","modelSource":{"copyFrom":"arn:aws:bedrock:us-east-1::fm/m"}}"#,
    );
    let body = req.json_body();
    let resp = crate::inference_profiles::create_inference_profile(&state, &req, &body).unwrap();
    let b = body_json(&resp);
    let arn = b["inferenceProfileArn"].as_str().unwrap();

    crate::inference_profiles::get_inference_profile(&state, &req, arn).unwrap();
    let resp = crate::inference_profiles::list_inference_profiles(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["inferenceProfileSummaries"]
        .as_array()
        .unwrap()
        .is_empty());
    crate::inference_profiles::delete_inference_profile(&state, &req, arn).unwrap();
}

#[test]
fn prompt_router_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"promptRouterName":"rt","models":[{"modelArn":"arn:aws:bedrock:us-east-1::fm/m"}],"fallbackModel":{"modelArn":"arn:aws:bedrock:us-east-1::fm/m"},"routingCriteria":{"responseQualityDifference":0.5}}"#,
    );
    let body = req.json_body();
    let resp = crate::prompt_routers::create_prompt_router(&state, &req, &body).unwrap();
    let b = body_json(&resp);
    let arn = b["promptRouterArn"].as_str().unwrap();

    crate::prompt_routers::get_prompt_router(&state, &req, arn).unwrap();
    let resp = crate::prompt_routers::list_prompt_routers(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["promptRouterSummaries"].as_array().unwrap().is_empty());
    crate::prompt_routers::delete_prompt_router(&state, &req, arn).unwrap();
}

#[test]
fn customization_job_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"jobName":"ft","customModelName":"cm","roleArn":"arn:aws:iam::1:role/r","baseModelIdentifier":"m","trainingDataConfig":{"s3Uri":"s3://t"},"outputDataConfig":{"s3Uri":"s3://o"}}"#,
    );
    let body = req.json_body();
    let resp = crate::customization::create_model_customization_job(&state, &req, &body).unwrap();
    let b = body_json(&resp);
    let arn = b["jobArn"].as_str().unwrap();

    crate::customization::get_model_customization_job(&state, &req, arn).unwrap();
    let resp = crate::customization::list_model_customization_jobs(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["modelCustomizationJobSummaries"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn provisioned_throughput_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"provisionedModelName":"pt","modelId":"m","modelUnits":1}"#,
    );
    let body = req.json_body();
    let resp = crate::throughput::create_provisioned_model_throughput(&state, &req, &body).unwrap();
    let b = body_json(&resp);
    let arn = b["provisionedModelArn"].as_str().unwrap();

    crate::throughput::get_provisioned_model_throughput(&state, &req, arn).unwrap();
    let resp = crate::throughput::list_provisioned_model_throughputs(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["provisionedModelSummaries"]
        .as_array()
        .unwrap()
        .is_empty());

    let upd = serde_json::json!({"desiredModelUnits": 2});
    crate::throughput::update_provisioned_model_throughput(&state, &req, arn, &upd).unwrap();
    crate::throughput::delete_provisioned_model_throughput(&state, &req, arn).unwrap();
}

#[test]
fn marketplace_endpoint_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"endpointName":"ep","modelSourceIdentifier":"arn:aws:sm:us-east-1:1:mp/p","endpointConfig":{"sageMaker":{"initialInstanceCount":1,"instanceType":"ml.g5.xlarge"}}}"#,
    );
    let body = req.json_body();
    let resp = crate::marketplace::create_marketplace_model_endpoint(&state, &req, &body).unwrap();
    let b = body_json(&resp);
    let arn = b["marketplaceModelEndpoint"]["endpointArn"]
        .as_str()
        .unwrap();

    crate::marketplace::get_marketplace_model_endpoint(&state, &req, arn).unwrap();
    let resp = crate::marketplace::list_marketplace_model_endpoints(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["marketplaceModelEndpoints"]
        .as_array()
        .unwrap()
        .is_empty());
    crate::marketplace::delete_marketplace_model_endpoint(&state, &req, arn).unwrap();
}

#[test]
fn async_invoke_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"modelId":"m","modelInput":{"prompt":"t"},"outputDataConfig":{"s3OutputDataConfig":{"s3Uri":"s3://o"}}}"#,
    );
    let body = req.json_body();
    let resp = crate::async_invoke::start_async_invoke(&state, &req, &body).unwrap();
    let b = body_json(&resp);
    let arn = b["invocationArn"].as_str().unwrap();

    crate::async_invoke::get_async_invoke(&state, &req, arn).unwrap();
    let resp = crate::async_invoke::list_async_invokes(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["asyncInvokeSummaries"].as_array().unwrap().is_empty());
}

#[test]
fn automated_reasoning_policy_crud() {
    let state = make_state();
    let req = make_request(
        Method::POST,
        "/",
        r#"{"policyName":"pol","policyDocument":{"rules":[]}}"#,
    );
    let body = req.json_body();
    let resp =
        crate::automated_reasoning::create_automated_reasoning_policy(&state, &req, &body).unwrap();
    let b = body_json(&resp);
    let arn = b["policyArn"].as_str().unwrap();

    crate::automated_reasoning::get_automated_reasoning_policy(&state, &req, arn).unwrap();
    let resp = crate::automated_reasoning::list_automated_reasoning_policies(&state, &req).unwrap();
    let b = body_json(&resp);
    assert!(!b["automatedReasoningPolicySummaries"]
        .as_array()
        .unwrap()
        .is_empty());

    let upd = serde_json::json!({"description": "updated"});
    crate::automated_reasoning::update_automated_reasoning_policy(&state, &req, arn, &upd).unwrap();
    crate::automated_reasoning::delete_automated_reasoning_policy(&state, &req, arn).unwrap();
}

#[test]
fn foundation_model_agreement_and_use_case() {
    let state = make_state();
    let req = make_request(Method::POST, "/", r#"{"modelId":"m","offerToken":"t"}"#);
    let body = req.json_body();
    crate::foundation_model_agreements::create_foundation_model_agreement(&state, &req, &body)
        .unwrap();

    crate::foundation_model_agreements::get_use_case_for_model_access(&state, &req).unwrap();
}

#[test]
fn enforced_guardrail_config() {
    let state = make_state();
    let body =
        serde_json::json!({"guardrailIdentifier":"g1","guardrailVersion":"1","modelArn":"arn:m"});
    let req = make_request(Method::POST, "/", "{}");
    crate::enforced_guardrails::put_enforced_guardrail_configuration(&state, &req, &body).unwrap();

    let req = make_request(Method::GET, "/", "");
    crate::enforced_guardrails::list_enforced_guardrails_configuration(&state, &req).unwrap();
}

#[tokio::test]
async fn unknown_route_returns_error_b() {
    let state = make_state();
    let svc = BedrockService::new(state);
    let req = make_request(Method::POST, "/unknown/route", "");
    assert!(svc.handle(req).await.is_err());
}

#[test]
fn automated_reasoning_policy_not_found_get() {
    let state = make_state();
    let req = make_request(Method::GET, "/", "{}");
    let result = crate::automated_reasoning::get_automated_reasoning_policy(
        &state,
        &req,
        "arn:aws:bedrock:us-east-1:123:automated-reasoning-policy/ghost",
    );
    assert!(result.is_err());
}

#[test]
fn automated_reasoning_policy_delete_not_found() {
    let state = make_state();
    let req = make_request(Method::DELETE, "/", "{}");
    let result = crate::automated_reasoning::delete_automated_reasoning_policy(
        &state,
        &req,
        "arn:aws:bedrock:us-east-1:123:automated-reasoning-policy/ghost",
    );
    assert!(result.is_err());
}

/// No snapshot store (memory mode) -> no persist hook for the CFN provisioner.
#[test]
fn snapshot_hook_is_none_without_store() {
    let state = make_state();
    let svc = BedrockService::new(state);
    assert!(svc.snapshot_hook().is_none());
}

#[tokio::test]
async fn snapshot_hook_fires_with_store() {
    let state = make_state();
    let store: Arc<dyn fakecloud_persistence::SnapshotStore> =
        Arc::new(fakecloud_persistence::MemorySnapshotStore::new());
    let svc = BedrockService::new(state).with_snapshot_store(store);
    let hook = svc
        .snapshot_hook()
        .expect("hook present when a store is set");
    hook().await;
}

#[tokio::test]
async fn create_guardrail_persists_tags_kms_and_cross_region() {
    let svc = BedrockService::new(make_state());
    let body = serde_json::json!({
        "name": "g",
        "blockedInputMessaging": "no",
        "blockedOutputsMessaging": "no",
        "kmsKeyId": "1234abcd-12ab-34cd-56ef-1234567890ab",
        "crossRegionConfig": {"guardrailProfileIdentifier": "us.guardrail.v1:0"},
        "automatedReasoningPolicyConfig": {
            "policies": ["arn:aws:bedrock:us-east-1:123456789012:automated-reasoning-policy/p1"],
            "confidenceThreshold": 0.5
        },
        "tags": [{"key": "env", "value": "prod"}],
    });
    let resp = svc
        .handle(make_request(Method::POST, "/guardrails", &body.to_string()))
        .await
        .unwrap();
    let b = body_json(&resp);
    let id = b["guardrailId"].as_str().unwrap().to_string();
    let arn = b["guardrailArn"].as_str().unwrap().to_string();

    let resp = svc
        .handle(make_request(Method::GET, &format!("/guardrails/{id}"), ""))
        .await
        .unwrap();
    let g = body_json(&resp);
    assert_eq!(
        g["kmsKeyArn"],
        "arn:aws:kms:us-east-1:123456789012:key/1234abcd-12ab-34cd-56ef-1234567890ab"
    );
    assert_eq!(
        g["crossRegionDetails"]["guardrailProfileId"],
        "us.guardrail.v1:0"
    );
    assert_eq!(
        g["crossRegionDetails"]["guardrailProfileArn"],
        "arn:aws:bedrock:us-east-1:123456789012:guardrail-profile/us.guardrail.v1:0"
    );
    assert_eq!(g["automatedReasoningPolicy"]["confidenceThreshold"], 0.5);

    let body = serde_json::json!({"resourceARN": arn});
    let resp = svc
        .handle(make_request(
            Method::POST,
            "/listTagsForResource",
            &body.to_string(),
        ))
        .await
        .unwrap();
    let tags = body_json(&resp)["tags"].clone();
    assert_eq!(tags, serde_json::json!([{"key": "env", "value": "prod"}]));

    // A published version carries the same members.
    let resp = svc
        .handle(make_request(
            Method::POST,
            &format!("/guardrails/{id}"),
            "{}",
        ))
        .await
        .unwrap();
    let ver = body_json(&resp)["version"].as_str().unwrap().to_string();
    let mut q = HashMap::new();
    q.insert("guardrailVersion".to_string(), ver);
    let resp = svc
        .handle(make_request_with_query(
            Method::GET,
            &format!("/guardrails/{id}"),
            "",
            q,
        ))
        .await
        .unwrap();
    let v = body_json(&resp);
    assert_eq!(g["kmsKeyArn"], v["kmsKeyArn"]);
    assert_eq!(g["crossRegionDetails"], v["crossRegionDetails"]);
}

#[tokio::test]
async fn create_custom_model_stores_model_tags() {
    let svc = BedrockService::new(make_state());
    let body = serde_json::json!({
        "modelName": "m",
        "modelSourceConfig": {"s3DataSource": {"s3Uri": "s3://b/k"}},
        "modelKmsKeyArn": "arn:aws:kms:us-east-1:123456789012:key/k1",
        "modelTags": [{"key": "team", "value": "ml"}],
    });
    let resp = svc
        .handle(make_request(
            Method::POST,
            "/custom-models/create-custom-model",
            &body.to_string(),
        ))
        .await
        .unwrap();
    let arn = body_json(&resp)["modelArn"].as_str().unwrap().to_string();
    let body = serde_json::json!({"resourceARN": arn});
    let resp = svc
        .handle(make_request(
            Method::POST,
            "/listTagsForResource",
            &body.to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(
        body_json(&resp)["tags"],
        serde_json::json!([{"key": "team", "value": "ml"}])
    );
}

// ── requestMetadata ──

fn recorded_metadata(
    state: &SharedBedrockState,
) -> Vec<Option<std::collections::BTreeMap<String, String>>> {
    let accts = state.read();
    accts
        .get("123456789012")
        .map(|s| {
            s.invocations
                .iter()
                .map(|i| i.request_metadata.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn with_metadata_header(mut req: AwsRequest, value: &str) -> AwsRequest {
    req.headers.insert(
        "x-amzn-bedrock-request-metadata",
        http::HeaderValue::from_str(value).unwrap(),
    );
    req
}

#[tokio::test]
async fn invoke_model_request_metadata_header_over_8500_is_rejected() {
    let body = serde_json::json!({"inputText": "Hello"}).to_string();
    for path in [
        "/model/amazon.titan-text-express-v1/invoke",
        "/model/amazon.titan-text-express-v1/invoke-with-response-stream",
    ] {
        let state = make_state();
        let svc = BedrockService::new(state.clone());
        // Valid JSON padded with insignificant whitespace: the cap is on the
        // header string, independent of the pairs it encodes.
        let padded = |len: usize| {
            let mut v = r#"{"k":"v"}"#.to_string();
            v.push_str(&" ".repeat(len - v.len()));
            v
        };
        let req = with_metadata_header(make_request(Method::POST, path, &body), &padded(8501));
        let err = expect_err(svc.handle(req).await);
        assert_eq!(err.code(), "ValidationException", "{path}");
        assert!(recorded_metadata(&state).is_empty(), "{path}");

        // Exactly at the cap is accepted.
        let req = with_metadata_header(make_request(Method::POST, path, &body), &padded(8500));
        svc.handle(req).await.unwrap();
    }
}

#[tokio::test]
async fn invoke_model_request_metadata_non_ascii_header_is_still_bounded() {
    let svc = BedrockService::new(make_state());
    let body = serde_json::json!({"inputText": "Hello"}).to_string();
    let mut req = make_request(
        Method::POST,
        "/model/amazon.titan-text-express-v1/invoke",
        &body,
    );
    // obs-text bytes (0x80..0xFF) make `HeaderValue::to_str` fail; the length
    // check must still run on the raw bytes.
    let mut raw = b"{\"k\":\"v\"}".to_vec();
    raw.extend(std::iter::repeat_n(0xE9u8, 8600));
    req.headers.insert(
        "x-amzn-bedrock-request-metadata",
        http::HeaderValue::from_bytes(&raw).unwrap(),
    );
    let err = expect_err(svc.handle(req).await);
    assert_eq!(err.code(), "ValidationException");
    assert!(err.message().contains("8500"), "{}", err.message());
}

#[tokio::test]
async fn invoke_model_malformed_request_metadata_header_is_rejected() {
    let body = serde_json::json!({"inputText": "Hello"}).to_string();
    let seventeen: serde_json::Map<String, Value> = (0..17)
        .map(|i| (format!("k{i}"), Value::String("v".into())))
        .collect();
    for header in [
        "not json".to_string(),
        r#"["a","b"]"#.to_string(),
        r#""just a string""#.to_string(),
        r#"{"n":1}"#.to_string(),
        r#"{"nested":{"a":"b"}}"#.to_string(),
        "{}".to_string(),
        Value::Object(seventeen).to_string(),
        format!(r#"{{"{}":"v"}}"#, "k".repeat(257)),
        format!(r#"{{"k":"{}"}}"#, "v".repeat(257)),
        r#"{"bad*key":"v"}"#.to_string(),
        r#"{"k":"bad<value>"}"#.to_string(),
        r#"{"":"v"}"#.to_string(),
    ] {
        let state = make_state();
        let svc = BedrockService::new(state.clone());
        let req = with_metadata_header(
            make_request(
                Method::POST,
                "/model/amazon.titan-text-express-v1/invoke",
                &body,
            ),
            &header,
        );
        let err = expect_err(svc.handle(req).await);
        assert_eq!(err.code(), "ValidationException", "{header}");
        assert!(recorded_metadata(&state).is_empty(), "{header}");
    }

    // The model's allowed character set (incl. the `,-.` range, whitespace,
    // and an empty value) is accepted.
    let state = make_state();
    let svc = BedrockService::new(state.clone());
    let req = with_metadata_header(
        make_request(
            Method::POST,
            "/model/amazon.titan-text-express-v1/invoke",
            &body,
        ),
        r#"{"a b:_@$#=/+,-.":"x y:_@$#=/+,-.","empty":""}"#,
    );
    svc.handle(req).await.unwrap();
    let recorded = recorded_metadata(&state);
    let m = recorded[0].as_ref().unwrap();
    assert_eq!(m.get("empty").map(String::as_str), Some(""));
    assert_eq!(
        m.get("a b:_@$#=/+,-.").map(String::as_str),
        Some("x y:_@$#=/+,-.")
    );
}

#[tokio::test]
async fn converse_request_metadata_map_constraints_are_enforced() {
    let seventeen: serde_json::Map<String, Value> = (0..17)
        .map(|i| (format!("k{i}"), Value::String("v".into())))
        .collect();
    for (path, meta) in [
        ("/model/anthropic.claude-v2/converse", serde_json::json!({})),
        (
            "/model/anthropic.claude-v2/converse",
            Value::Object(seventeen.clone()),
        ),
        (
            "/model/anthropic.claude-v2/converse",
            serde_json::json!({"k": 7}),
        ),
        (
            "/model/anthropic.claude-v2/converse-stream",
            serde_json::json!({"bad*key": "v"}),
        ),
        (
            "/model/anthropic.claude-v2/converse-stream",
            serde_json::json!({"k": "v".repeat(257)}),
        ),
        (
            "/model/anthropic.claude-v2/converse-stream",
            serde_json::json!(["k", "v"]),
        ),
    ] {
        let state = make_state();
        let svc = BedrockService::new(state.clone());
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": [{"text": "Hello"}]}],
            "requestMetadata": meta
        })
        .to_string();
        let err = expect_err(svc.handle(make_request(Method::POST, path, &body)).await);
        assert_eq!(err.code(), "ValidationException", "{path} {meta}");
        assert!(recorded_metadata(&state).is_empty(), "{path} {meta}");
    }

    // ConverseStream records a valid map.
    let state = make_state();
    let svc = BedrockService::new(state.clone());
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "Hello"}]}],
        "requestMetadata": {"tenant": "acme"}
    })
    .to_string();
    svc.handle(make_request(
        Method::POST,
        "/model/anthropic.claude-v2/converse-stream",
        &body,
    ))
    .await
    .unwrap();
    assert_eq!(
        recorded_metadata(&state)[0]
            .as_ref()
            .and_then(|m| m.get("tenant"))
            .map(String::as_str),
        Some("acme")
    );
}

#[tokio::test]
async fn invoke_model_records_request_metadata_header_pairs() {
    let state = make_state();
    let svc = BedrockService::new(state.clone());
    let body = serde_json::json!({
        "inputText": "Hello",
        // The InvokeModel body is the opaque model payload: a
        // `requestMetadata` key in it is not request metadata.
        "requestMetadata": {"from": "body"}
    })
    .to_string();
    let req = with_metadata_header(
        make_request(
            Method::POST,
            "/model/amazon.titan-text-express-v1/invoke",
            &body,
        ),
        r#"{"team":"search","env":"dev"}"#,
    );
    svc.handle(req).await.unwrap();
    let req = make_request(
        Method::POST,
        "/model/amazon.titan-text-express-v1/invoke",
        &body,
    );
    svc.handle(req).await.unwrap();

    let recorded = recorded_metadata(&state);
    assert_eq!(recorded.len(), 2);
    let first = recorded[0].as_ref().unwrap();
    assert_eq!(first.get("team").map(String::as_str), Some("search"));
    assert_eq!(first.get("env").map(String::as_str), Some("dev"));
    assert_eq!(first.len(), 2);
    assert!(recorded[1].is_none());
}

#[tokio::test]
async fn converse_records_request_metadata_map() {
    let state = make_state();
    let svc = BedrockService::new(state.clone());
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "Hello"}]}],
        "requestMetadata": {"tenant": "acme"}
    })
    .to_string();
    let req = make_request(Method::POST, "/model/anthropic.claude-v2/converse", &body);
    svc.handle(req).await.unwrap();

    let recorded = recorded_metadata(&state);
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0]
            .as_ref()
            .and_then(|m| m.get("tenant"))
            .map(String::as_str),
        Some("acme")
    );
}

// ── InvokeGuardrailChecks ──

#[tokio::test]
async fn invoke_guardrail_checks_routes_and_evaluates() {
    let svc = BedrockService::new(make_state());
    assert!(svc.supported_actions().contains(&"InvokeGuardrailChecks"));
    let body = serde_json::json!({
        "messages": [{"role": "user", "content": [{"text": "call 555-123-4567"}]}],
        "checks": {"sensitiveInformation": {"entities": [{"type": "PHONE"}]}}
    });
    let req = make_request(Method::POST, "/guardrail-checks/invoke", &body.to_string());
    let resp = svc.handle(req).await.unwrap();
    let b = body_json(&resp);
    let hit = &b["results"]["sensitiveInformation"]["results"][0];
    assert_eq!(hit["type"], "PHONE");
    assert_eq!(hit["beginOffset"], 5);
    assert_eq!(hit["endOffset"], 17);

    let req = make_request(
        Method::POST,
        "/guardrail-checks/invoke",
        r#"{"messages":[],"checks":{}}"#,
    );
    assert_eq!(
        expect_err(svc.handle(req).await).code(),
        "ValidationException"
    );
}
