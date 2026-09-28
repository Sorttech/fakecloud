//! ARN builders for Bedrock Agents runtime resources. Each ARN takes the
//! partition of the region it is minted in.

use fakecloud_aws::arn::Arn;

fn effective_region(region: &str) -> &str {
    if region.is_empty() {
        "us-east-1"
    } else {
        region
    }
}

fn bedrock_arn(region: &str, account_id: &str, resource: &str) -> String {
    Arn::regional("bedrock", effective_region(region), account_id, resource).to_string()
}

pub fn session_arn(region: &str, account_id: &str, session_id: &str) -> String {
    bedrock_arn(region, account_id, &format!("session/{session_id}"))
}

pub fn flow_execution_arn(
    region: &str,
    account_id: &str,
    flow_id: &str,
    execution_id: &str,
) -> String {
    bedrock_arn(
        region,
        account_id,
        &format!("flow/{flow_id}/execution/{execution_id}"),
    )
}
