//! Bedrock Agents Runtime flow executions run real Bedrock Agents flows: an
//! execution targets an existing flow alias, runs the version that alias routes
//! to, and captures that version's definition and role in its snapshot.
//! Unknown flows, aliases and executions are `ResourceNotFoundException`.

mod helpers;

use aws_sdk_bedrockagent::types::{
    FlowAliasRoutingConfigurationListItem, FlowDefinition, FlowNode, FlowNodeConfiguration,
    FlowNodeType, InputFlowNodeConfiguration,
};
use aws_sdk_bedrockagentruntime::types::FlowExecutionStatus;
use helpers::TestServer;

const ROLE: &str = "arn:aws:iam::123456789012:role/service-role/flow-exec-role";

fn definition(node: &str) -> FlowDefinition {
    FlowDefinition::builder()
        .nodes(
            FlowNode::builder()
                .name(node)
                .r#type(FlowNodeType::Input)
                .configuration(FlowNodeConfiguration::Input(
                    InputFlowNodeConfiguration::builder().build(),
                ))
                .build()
                .expect("flow node"),
        )
        .build()
}

fn is_not_found<E>(err: &aws_sdk_bedrockagentruntime::error::SdkError<E>) -> bool
where
    E: std::fmt::Debug + aws_sdk_bedrockagentruntime::error::ProvideErrorMetadata,
{
    err.as_service_error().and_then(|e| e.code()) == Some("ResourceNotFoundException")
}

#[tokio::test]
async fn flow_executions_run_the_aliased_version_and_capture_it() {
    let server = TestServer::start().await;
    let agent = server.bedrock_agent_client().await;
    let runtime = server.bedrock_agent_runtime_client().await;

    // A flow whose version 1 differs from the later-edited draft.
    let flow_id = agent
        .create_flow()
        .name("e2e-flow")
        .execution_role_arn(ROLE)
        .definition(definition("VersionOneInput"))
        .send()
        .await
        .expect("create flow")
        .id;
    agent
        .prepare_flow()
        .flow_identifier(&flow_id)
        .send()
        .await
        .expect("prepare flow");
    let version = agent
        .create_flow_version()
        .flow_identifier(&flow_id)
        .send()
        .await
        .expect("create flow version")
        .version;
    assert_eq!(version, "1");
    let alias_id = agent
        .create_flow_alias()
        .flow_identifier(&flow_id)
        .name("live")
        .routing_configuration(
            FlowAliasRoutingConfigurationListItem::builder()
                .flow_version("1")
                .build(),
        )
        .send()
        .await
        .expect("create flow alias")
        .id;
    agent
        .update_flow()
        .flow_identifier(&flow_id)
        .name("e2e-flow")
        .execution_role_arn(ROLE)
        .definition(definition("DraftInput"))
        .send()
        .await
        .expect("update draft");

    // The alias runs version 1, not the edited draft.
    let exec_arn = runtime
        .start_flow_execution()
        .flow_identifier(&flow_id)
        .flow_alias_identifier(&alias_id)
        .set_inputs(Some(Vec::new()))
        .send()
        .await
        .expect("start flow execution")
        .execution_arn
        .expect("execution arn");
    assert!(
        exec_arn.contains(&format!(":flow/{flow_id}/alias/{alias_id}/execution/")),
        "{exec_arn}"
    );
    let snapshot = runtime
        .get_execution_flow_snapshot()
        .flow_identifier(&flow_id)
        .flow_alias_identifier(&alias_id)
        .execution_identifier(&exec_arn)
        .send()
        .await
        .expect("get snapshot");
    assert_eq!(snapshot.flow_version(), "1");
    assert_eq!(snapshot.execution_role_arn(), ROLE);
    assert!(
        snapshot.definition().contains("VersionOneInput"),
        "{}",
        snapshot.definition()
    );
    assert!(!snapshot.definition().contains("DraftInput"));

    // The test alias runs the draft.
    let draft_exec = runtime
        .start_flow_execution()
        .flow_identifier(&flow_id)
        .flow_alias_identifier("TSTALIASID")
        .set_inputs(Some(Vec::new()))
        .send()
        .await
        .expect("start draft execution")
        .execution_arn
        .expect("execution arn");
    let draft = runtime
        .get_execution_flow_snapshot()
        .flow_identifier(&flow_id)
        .flow_alias_identifier("TSTALIASID")
        .execution_identifier(&draft_exec)
        .send()
        .await
        .expect("get draft snapshot");
    assert_eq!(draft.flow_version(), "DRAFT");
    assert!(draft.definition().contains("DraftInput"));

    // Stop aborts the running execution.
    let stopped = runtime
        .stop_flow_execution()
        .flow_identifier(&flow_id)
        .flow_alias_identifier(&alias_id)
        .execution_identifier(&exec_arn)
        .send()
        .await
        .expect("stop execution");
    assert_eq!(stopped.status(), &FlowExecutionStatus::Aborted);
    let got = runtime
        .get_flow_execution()
        .flow_identifier(&flow_id)
        .flow_alias_identifier(&alias_id)
        .execution_identifier(&exec_arn)
        .send()
        .await
        .expect("get execution");
    assert_eq!(got.status(), &FlowExecutionStatus::Aborted);
    assert_eq!(got.flow_version(), "1");
    assert!(got.ended_at().is_some());
}

#[tokio::test]
async fn unknown_flows_and_executions_are_not_found() {
    let server = TestServer::start().await;
    let agent = server.bedrock_agent_client().await;
    let runtime = server.bedrock_agent_runtime_client().await;

    let err = runtime
        .start_flow_execution()
        .flow_identifier("NOSUCHFLOW")
        .flow_alias_identifier("TSTALIASID")
        .set_inputs(Some(Vec::new()))
        .send()
        .await
        .expect_err("unknown flow");
    assert!(is_not_found(&err), "{err:?}");

    let flow_id = agent
        .create_flow()
        .name("e2e-flow-2")
        .execution_role_arn(ROLE)
        .definition(definition("In"))
        .send()
        .await
        .expect("create flow")
        .id;
    let err = runtime
        .start_flow_execution()
        .flow_identifier(&flow_id)
        .flow_alias_identifier("NOSUCHALIA")
        .set_inputs(Some(Vec::new()))
        .send()
        .await
        .expect_err("unknown alias");
    assert!(is_not_found(&err), "{err:?}");

    let err = runtime
        .get_execution_flow_snapshot()
        .flow_identifier(&flow_id)
        .flow_alias_identifier("TSTALIASID")
        .execution_identifier("no-such-execution")
        .send()
        .await
        .expect_err("unknown execution snapshot");
    assert!(is_not_found(&err), "{err:?}");

    let err = runtime
        .stop_flow_execution()
        .flow_identifier(&flow_id)
        .flow_alias_identifier("TSTALIASID")
        .execution_identifier("no-such-execution")
        .send()
        .await
        .expect_err("unknown execution stop");
    assert!(is_not_found(&err), "{err:?}");
}
