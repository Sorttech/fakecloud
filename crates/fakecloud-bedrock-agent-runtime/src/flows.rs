//! The flow an execution runs: resolving a `flowIdentifier` +
//! `flowAliasIdentifier` pair against Bedrock Agents state to the flow version
//! the alias routes to, and the definition, execution role and customer key
//! that version carries. An execution captures this at start, so its snapshot
//! stays what actually ran even if the flow is later edited.

use http::StatusCode;
use serde_json::Value;

use fakecloud_bedrock_agent::SharedBedrockAgentState;
use fakecloud_core::service::AwsServiceError;

/// The built-in alias every flow has, routing to its working draft.
pub(crate) const TEST_ALIAS_ID: &str = "TSTALIASID";

const DRAFT: &str = "DRAFT";

/// What an execution of a flow alias runs.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedFlow {
    pub flow_id: String,
    pub alias_id: String,
    pub version: String,
    pub definition: Option<Value>,
    pub execution_role_arn: Option<String>,
    pub customer_encryption_key_arn: Option<String>,
}

pub(crate) fn not_found(message: impl Into<String>) -> AwsServiceError {
    AwsServiceError::aws_error(
        StatusCode::NOT_FOUND,
        "ResourceNotFoundException",
        message.into(),
    )
}

/// The `region:account:flow/...` part of a Bedrock flow ARN, or `None` when
/// the identifier is a bare id.
fn flow_arn_rest(identifier: &str) -> Option<&str> {
    fakecloud_aws::arn::arn_resource(identifier, "bedrock")
}

/// The account a flow ARN names, `None` for a bare id.
fn arn_account(identifier: &str) -> Option<&str> {
    flow_arn_rest(identifier).and_then(|rest| rest.split(':').nth(1))
}

/// The bare flow id a `flowIdentifier` names: the identifier itself, or the id
/// inside a flow ARN (`arn:<partition>:bedrock:<region>:<account>:flow/<id>`).
pub(crate) fn bare_flow_id(identifier: &str) -> &str {
    flow_arn_rest(identifier)
        .and_then(|rest| rest.split_once(":flow/"))
        .map_or(identifier, |(_, id)| id.split('/').next().unwrap_or(id))
}

/// The bare alias id a `flowAliasIdentifier` names for `flow_id`: the
/// identifier itself, or the alias id inside an alias ARN
/// (`...:flow/<flowId>/alias/<aliasId>`) whose flow is `flow_id`.
fn bare_alias_id(identifier: &str, flow_id: &str) -> Option<String> {
    match flow_arn_rest(identifier) {
        None => Some(identifier.to_string()),
        Some(rest) => rest
            .split_once(":flow/")
            .and_then(|(_, path)| path.split_once("/alias/"))
            .filter(|(flow, alias)| *flow == flow_id && !alias.is_empty())
            .map(|(_, alias)| alias.to_string()),
    }
}

/// Resolve the flow and alias an execution targets in `account_id`. Every
/// miss (unknown flow, an identifier ARN in another account, unknown alias or
/// one belonging to another flow, a routed version that no longer exists) is a
/// `ResourceNotFoundException`, as the real service reports.
pub(crate) fn resolve_flow(
    agent_state: Option<&SharedBedrockAgentState>,
    account_id: &str,
    flow_identifier: &str,
    alias_identifier: &str,
) -> Result<ResolvedFlow, AwsServiceError> {
    let flow_id = bare_flow_id(flow_identifier).to_string();
    let flow_missing = || not_found(format!("Flow {flow_identifier} not found."));
    if arn_account(flow_identifier).is_some_and(|a| a != account_id) {
        return Err(flow_missing());
    }
    let alias_missing = || not_found(format!("Flow alias {alias_identifier} not found."));
    if arn_account(alias_identifier).is_some_and(|a| a != account_id) {
        return Err(alias_missing());
    }
    let alias_id = bare_alias_id(alias_identifier, &flow_id).ok_or_else(alias_missing)?;

    let agent_state = agent_state.ok_or_else(flow_missing)?;
    let accounts = agent_state.read();
    let state = accounts.get(account_id).ok_or_else(flow_missing)?;
    let flow = state.flows.get(&flow_id).ok_or_else(flow_missing)?;

    let version = if alias_id == TEST_ALIAS_ID {
        DRAFT.to_string()
    } else {
        let alias = state
            .flow_aliases
            .get(&alias_id)
            .filter(|a| a.flow_id == flow_id)
            .ok_or_else(alias_missing)?;
        alias
            .routing_configuration
            .iter()
            .find_map(|r| r.get("flowVersion").and_then(Value::as_str))
            .unwrap_or(DRAFT)
            .to_string()
    };

    let resolved = if version == DRAFT {
        ResolvedFlow {
            flow_id,
            alias_id,
            version,
            definition: flow.definition.clone(),
            execution_role_arn: flow.execution_role_arn.clone(),
            customer_encryption_key_arn: flow.customer_encryption_key_arn.clone(),
        }
    } else {
        let fv = state
            .flow_versions
            .get(&flow_id)
            .and_then(|versions| versions.iter().find(|v| v.flow_version == version))
            .ok_or_else(|| not_found(format!("Flow version {version} not found.")))?;
        // Versions captured before their flow-level fields were stored fall
        // back to the parent flow, as the Bedrock Agents readers do.
        ResolvedFlow {
            flow_id,
            alias_id,
            version,
            definition: fv.definition.clone(),
            execution_role_arn: fv
                .execution_role_arn
                .clone()
                .or_else(|| flow.execution_role_arn.clone()),
            customer_encryption_key_arn: fv
                .customer_encryption_key_arn
                .clone()
                .or_else(|| flow.customer_encryption_key_arn.clone()),
        }
    };
    Ok(resolved)
}

/// Whether `identifier` (an execution id or execution ARN) names the execution
/// `execution_id` / `execution_arn`.
pub(crate) fn names_execution(identifier: &str, execution_id: &str, execution_arn: &str) -> bool {
    identifier == execution_id || identifier == execution_arn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_parse_to_bare_ids() {
        assert_eq!(bare_flow_id("ABCDEFGHIJ"), "ABCDEFGHIJ");
        assert_eq!(
            bare_flow_id("arn:aws-cn:bedrock:cn-north-1:123456789012:flow/ABCDEFGHIJ"),
            "ABCDEFGHIJ"
        );
        assert_eq!(
            bare_alias_id(
                "arn:aws:bedrock:us-east-1:123456789012:flow/ABCDEFGHIJ/alias/KLMNOPQRST",
                "ABCDEFGHIJ"
            )
            .as_deref(),
            Some("KLMNOPQRST")
        );
        // An alias ARN of another flow names no alias of this one.
        assert_eq!(
            bare_alias_id(
                "arn:aws:bedrock:us-east-1:123456789012:flow/ZZZZZZZZZZ/alias/KLMNOPQRST",
                "ABCDEFGHIJ"
            ),
            None
        );
        assert_eq!(
            arn_account("arn:aws:bedrock:us-east-1:111122223333:flow/ABCDEFGHIJ"),
            Some("111122223333")
        );
        assert_eq!(arn_account("ABCDEFGHIJ"), None);
    }
}
