//! ARN builders for Bedrock Agents resources and the default roles they are
//! created with. Each ARN takes the partition of the region it is minted in.

use fakecloud_aws::arn::Arn;

fn bedrock_arn(region: &str, account_id: &str, resource: &str) -> String {
    Arn::regional("bedrock", region, account_id, resource).to_string()
}

fn role_arn(region: &str, account_id: &str, path_and_name: &str) -> String {
    Arn::global_in(region, "iam", account_id, &format!("role/{path_and_name}")).to_string()
}

pub fn agent_arn(region: &str, account_id: &str, agent_id: &str) -> String {
    bedrock_arn(region, account_id, &format!("agent/{agent_id}"))
}

pub fn agent_alias_arn(region: &str, account_id: &str, agent_id: &str, alias_id: &str) -> String {
    bedrock_arn(
        region,
        account_id,
        &format!("agent-alias/{agent_id}/{alias_id}"),
    )
}

pub fn knowledge_base_arn(region: &str, account_id: &str, knowledge_base_id: &str) -> String {
    bedrock_arn(
        region,
        account_id,
        &format!("knowledge-base/{knowledge_base_id}"),
    )
}

pub fn flow_arn(region: &str, account_id: &str, flow_id: &str) -> String {
    bedrock_arn(region, account_id, &format!("flow/{flow_id}"))
}

pub fn prompt_arn(region: &str, account_id: &str, prompt_id: &str) -> String {
    bedrock_arn(region, account_id, &format!("prompt/{prompt_id}"))
}

/// The role an agent runs as when CreateAgent names none.
pub fn default_agent_role_arn(region: &str, account_id: &str) -> String {
    role_arn(region, account_id, "fakecloud-bedrock-agent-role")
}

/// The role a knowledge base runs as when CreateKnowledgeBase names none.
pub fn default_knowledge_base_role_arn(region: &str, account_id: &str) -> String {
    role_arn(region, account_id, "fakecloud-bedrock-kb-role")
}

/// The execution role a flow runs as when CreateFlow names none.
pub fn default_flow_execution_role_arn(region: &str, account_id: &str, flow_id: &str) -> String {
    role_arn(
        region,
        account_id,
        &format!("service-role/AmazonBedrockExecutionRoleForFlows_{flow_id}"),
    )
}
