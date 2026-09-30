use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

pub type SharedBedrockAgentRuntimeState = Arc<RwLock<BedrockAgentRuntimeAccounts>>;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BedrockAgentRuntimeAccounts {
    pub accounts: BTreeMap<String, BedrockAgentRuntimeState>,
}

impl BedrockAgentRuntimeAccounts {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_create(&mut self, account_id: &str) -> &mut BedrockAgentRuntimeState {
        self.accounts
            .entry(account_id.to_string())
            .or_insert_with(|| BedrockAgentRuntimeState::new(account_id))
    }

    pub fn reset(&mut self) {
        self.accounts.clear();
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BedrockAgentRuntimeState {
    pub account_id: String,
    pub invocations: Vec<InvocationRecord>,
    pub sessions: BTreeMap<String, Session>,
    /// Keyed by `crate::flows::execution_map_key` (flow, alias, execution id).
    /// Loading older state re-keys entries stored by bare execution id and
    /// normalizes their status to the model's enum.
    #[serde(deserialize_with = "deserialize_flow_executions")]
    pub flow_executions: BTreeMap<String, FlowExecution>,
    /// Per-session list of invocations created via `CreateInvocation`
    /// (separate from `invocations` which is the data-plane invocation log).
    #[serde(default)]
    pub session_invocations: BTreeMap<String, Vec<SessionInvocation>>,
    /// Invocation steps keyed by `(sessionId, invocationStepId)`. Stored as a
    /// flat map so `GetInvocationStep` can look up by step id alone while
    /// `ListInvocationSteps` can filter by session/invocation.
    #[serde(default)]
    pub invocation_steps: BTreeMap<String, InvocationStep>,
    /// Tags keyed by resource ARN.
    #[serde(default)]
    pub tags: BTreeMap<String, BTreeMap<String, String>>,
}

impl BedrockAgentRuntimeState {
    pub fn new(account_id: &str) -> Self {
        Self {
            account_id: account_id.to_string(),
            invocations: Vec::new(),
            sessions: BTreeMap::new(),
            flow_executions: BTreeMap::new(),
            session_invocations: BTreeMap::new(),
            invocation_steps: BTreeMap::new(),
            tags: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationRecord {
    pub invocation_id: String,
    /// One of `invoke_agent`, `invoke_inline_agent`, `invoke_flow`,
    /// `retrieve`, `retrieve_and_generate`, `create_invocation`.
    pub op: String,
    pub agent_id: Option<String>,
    pub flow_id: Option<String>,
    pub session_id: Option<String>,
    pub input: String,
    pub output: String,
    /// Number of chunk frames (or retrieval results) emitted for this
    /// invocation. Always `>= 1` for eventstream ops, may be `0` for
    /// session-only `CreateInvocation` rows.
    pub output_chunks: u32,
    /// Optional trace blob captured for InvokeAgent-style ops. Stored as
    /// JSON so the introspection endpoint can hand it back unchanged.
    pub trace: Option<serde_json::Value>,
    /// Citations attached by RetrieveAndGenerate. Empty for ops that
    /// don't emit them.
    #[serde(default)]
    pub citations: Vec<serde_json::Value>,
    pub timestamp: DateTime<Utc>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub session_id: String,
    pub session_arn: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub encryption_key_arn: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInvocation {
    pub invocation_id: String,
    pub session_id: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationStep {
    pub session_id: String,
    pub invocation_id: String,
    pub invocation_step_id: String,
    pub invocation_step_time: DateTime<Utc>,
    pub payload: serde_json::Value,
}

/// Load flow executions, migrating older records: re-key each under its
/// (flow, alias, execution id) key and map legacy statuses (`InProgress`,
/// `SUCCEEDED`, ...) to the model's `FlowExecutionStatus` values, so Get and
/// Stop treat them like executions recorded today.
fn deserialize_flow_executions<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, FlowExecution>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let stored = BTreeMap::<String, FlowExecution>::deserialize(deserializer)?;
    Ok(stored
        .into_values()
        .map(|mut e| {
            e.status = crate::flows::normalize_status(&e.status);
            let key =
                crate::flows::execution_map_key(&e.flow_id, &e.flow_alias_id, &e.execution_id);
            (key, e)
        })
        .collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowExecution {
    pub execution_id: String,
    pub execution_arn: String,
    pub flow_id: String,
    #[serde(default)]
    pub flow_alias_id: String,
    #[serde(default)]
    pub flow_version: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    /// The flow definition the execution runs, captured at start from the
    /// version its alias routed to. `GetExecutionFlowSnapshot` returns it.
    #[serde(default)]
    pub definition: Option<serde_json::Value>,
    /// The executed version's service role, captured at start.
    #[serde(default)]
    pub execution_role_arn: Option<String>,
    /// The executed version's customer-managed KMS key, captured at start.
    #[serde(default)]
    pub customer_encryption_key_arn: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// State saved by an older build (executions keyed by bare id, non-model
    /// statuses) loads re-keyed with model statuses.
    #[test]
    fn legacy_flow_executions_migrate_on_load() {
        let snapshot = serde_json::json!({
            "accounts": {
                "123456789012": {
                    "account_id": "123456789012",
                    "invocations": [],
                    "sessions": {},
                    "flow_executions": {
                        "e-1": {
                            "execution_id": "e-1",
                            "execution_arn": "arn:aws:bedrock:us-east-1:123456789012:flow/FFFFFFFFFF/execution/e-1",
                            "flow_id": "FFFFFFFFFF",
                            "flow_alias_id": "TSTALIASID",
                            "flow_version": "DRAFT",
                            "status": "InProgress",
                            "created_at": "2026-01-01T00:00:00Z",
                            "updated_at": "2026-01-01T00:00:00Z"
                        },
                        "e-2": {
                            "execution_id": "e-2",
                            "execution_arn": "arn:aws:bedrock:us-east-1:123456789012:flow/FFFFFFFFFF/execution/e-2",
                            "flow_id": "FFFFFFFFFF",
                            "flow_alias_id": "TSTALIASID",
                            "status": "SUCCEEDED",
                            "created_at": "2026-01-01T00:00:00Z",
                            "updated_at": "2026-01-01T00:00:00Z"
                        }
                    }
                }
            }
        });
        let accounts: BedrockAgentRuntimeAccounts = serde_json::from_value(snapshot).unwrap();
        let execs = &accounts.accounts["123456789012"].flow_executions;
        assert_eq!(execs["FFFFFFFFFF/TSTALIASID/e-1"].status, "Running");
        assert_eq!(execs["FFFFFFFFFF/TSTALIASID/e-2"].status, "Succeeded");
        assert!(!execs.contains_key("e-1"));
    }

    #[test]
    fn invocation_record_serializes_introspection_fields() {
        let rec = InvocationRecord {
            invocation_id: "inv-1".into(),
            op: "invoke_agent".into(),
            agent_id: Some("agent-1".into()),
            flow_id: None,
            session_id: Some("sess-1".into()),
            input: "hi".into(),
            output: "hello".into(),
            output_chunks: 1,
            trace: Some(serde_json::json!({"orchestration": "ok"})),
            citations: vec![serde_json::json!({"ref": "doc1"})],
            timestamp: Utc::now(),
            duration_ms: 42,
        };
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["op"], "invoke_agent");
        assert_eq!(v["agent_id"], "agent-1");
        assert_eq!(v["session_id"], "sess-1");
        assert_eq!(v["output_chunks"], 1);
        assert_eq!(v["duration_ms"], 42);
        assert!(v["trace"].is_object());
        assert!(v["citations"].is_array());
    }
}
