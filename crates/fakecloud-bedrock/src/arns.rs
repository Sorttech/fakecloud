//! ARN builders for every Bedrock resource kind. Each ARN takes the partition
//! of the region it is minted in.

use fakecloud_aws::arn::Arn;

fn bedrock_arn(region: &str, account_id: &str, resource: &str) -> String {
    Arn::regional("bedrock", region, account_id, resource).to_string()
}

pub fn foundation_model_arn(region: &str, model_id: &str) -> String {
    bedrock_arn(region, "", &format!("foundation-model/{model_id}"))
}

pub fn custom_model_arn(region: &str, account_id: &str, model: &str) -> String {
    bedrock_arn(region, account_id, &format!("custom-model/{model}"))
}

pub fn imported_model_arn(region: &str, account_id: &str, model: &str) -> String {
    bedrock_arn(region, account_id, &format!("imported-model/{model}"))
}

pub fn provisioned_model_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(region, account_id, &format!("provisioned-model/{id}"))
}

pub fn custom_model_deployment_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(region, account_id, &format!("custom-model-deployment/{id}"))
}

pub fn marketplace_model_endpoint_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(
        region,
        account_id,
        &format!("marketplace-model-endpoint/{id}"),
    )
}

pub fn guardrail_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(region, account_id, &format!("guardrail/{id}"))
}

pub fn prompt_router_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(region, account_id, &format!("prompt-router/{id}"))
}

pub fn application_inference_profile_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(
        region,
        account_id,
        &format!("application-inference-profile/{id}"),
    )
}

pub fn inference_profile_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(region, account_id, &format!("inference-profile/{id}"))
}

pub fn automated_reasoning_policy_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(
        region,
        account_id,
        &format!("automated-reasoning-policy/{id}"),
    )
}

pub fn async_invoke_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(region, account_id, &format!("async-invoke/{id}"))
}

pub fn evaluation_job_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(region, account_id, &format!("evaluation-job/{id}"))
}

pub fn model_invocation_job_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(region, account_id, &format!("model-invocation-job/{id}"))
}

pub fn model_import_job_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(region, account_id, &format!("model-import-job/{id}"))
}

pub fn model_copy_job_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(region, account_id, &format!("model-copy-job/{id}"))
}

pub fn model_customization_job_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(region, account_id, &format!("model-customization-job/{id}"))
}

pub fn advanced_prompt_optimization_job_arn(region: &str, account_id: &str, id: &str) -> String {
    bedrock_arn(
        region,
        account_id,
        &format!("advanced-prompt-optimization-job/{id}"),
    )
}
