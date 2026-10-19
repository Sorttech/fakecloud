//! A container-backed `AWS::RDS::DBInstance` must not let its stack reach
//! CREATE_COMPLETE (or resolve its Outputs, Exports and dependent resources)
//! with the pre-container placeholder endpoint. CloudFormation only finishes an
//! RDS instance once it is available, so `Fn::GetAtt Db.Endpoint.Address` /
//! `Endpoint.Port` carry the endpoint DescribeDBInstances reports. Before the
//! fix the stack completed while the container was still booting, so the
//! Outputs, the cross-stack Export and a same-stack consumer all held the
//! placeholder and another stack's `Fn::ImportValue` dialled a host that does
//! not resolve.
//!
//! Covers CreateStack, the change-set path `cdk deploy` uses, and an
//! UpdateStack that adds the instance. With no container runtime the instance is `available` at insert and the endpoint
//! never moves, so the assertions hold trivially there.

mod helpers;

use aws_sdk_cloudformation::types::{ChangeSetType, Stack};
use helpers::TestServer;

fn template(id: &str) -> String {
    template_with_version(id, "16.3")
}

fn template_with_version(id: &str, engine_version: &str) -> String {
    format!(
        r#"{{
  "Resources": {{
    "Db": {{
      "Type": "AWS::RDS::DBInstance",
      "Properties": {{
        "DBInstanceIdentifier": "{id}",
        "DBInstanceClass": "db.t4g.micro",
        "Engine": "postgres",
        "EngineVersion": "{engine_version}",
        "MasterUsername": "admin",
        "MasterUserPassword": "hunter2-secret",
        "AllocatedStorage": "20"
      }}
    }},
    "Host": {{
      "Type": "AWS::SSM::Parameter",
      "Properties": {{
        "Name": "/{id}/host",
        "Type": "String",
        "Value": {{"Fn::GetAtt": ["Db", "Endpoint.Address"]}}
      }}
    }}
  }},
  "Outputs": {{
    "Address": {{
      "Value": {{"Fn::GetAtt": ["Db", "Endpoint.Address"]}},
      "Export": {{"Name": "{id}-address"}}
    }},
    "Port": {{
      "Value": {{"Fn::GetAtt": ["Db", "Endpoint.Port"]}},
      "Export": {{"Name": "{id}-port"}}
    }}
  }}
}}"#
    )
}

async fn wait_stack_terminal(cfn: &aws_sdk_cloudformation::Client, stack: &str) -> Stack {
    helpers::wait_until(std::time::Duration::from_secs(300), || async {
        let out = cfn.describe_stacks().stack_name(stack).send().await.ok()?;
        let s = out.stacks().first()?;
        (!s.stack_status()?.as_str().ends_with("IN_PROGRESS")).then(|| s.clone())
    })
    .await
    .expect("stack reached a terminal status")
}

fn output<'a>(stack: &'a Stack, key: &str) -> &'a str {
    stack
        .outputs()
        .iter()
        .find(|o| o.output_key() == Some(key))
        .and_then(|o| o.output_value())
        .unwrap_or_else(|| panic!("output {key} present"))
}

/// Once the stack is `complete` (CREATE_COMPLETE or UPDATE_COMPLETE), the
/// instance is available and every value derived from its endpoint matches
/// DescribeDBInstances.
async fn assert_real_endpoint(server: &TestServer, stack_name: &str, id: &str, complete: &str) {
    let cfn = server.cloudformation_client().await;
    let rds = server.rds_client().await;
    let ssm = server.ssm_client().await;

    let stack = wait_stack_terminal(&cfn, stack_name).await;
    assert_eq!(
        stack.stack_status().map(|s| s.as_str()),
        Some(complete),
        "reason: {:?}",
        stack.stack_status_reason()
    );

    let described = rds
        .describe_db_instances()
        .db_instance_identifier(id)
        .send()
        .await
        .expect("describe_db_instances");
    let inst = described.db_instances().first().expect("instance present");
    assert_eq!(inst.db_instance_status(), Some("available"));
    let endpoint = inst.endpoint().expect("endpoint");
    let address = endpoint.address().expect("address");
    let port = endpoint.port().expect("port").to_string();

    assert_eq!(output(&stack, "Address"), address, "Address output");
    assert_eq!(output(&stack, "Port"), port, "Port output");

    let exports = cfn.list_exports().send().await.expect("list_exports");
    let export = |name: String| {
        exports
            .exports()
            .iter()
            .find(|e| e.name() == Some(name.as_str()))
            .and_then(|e| e.value())
            .map(str::to_string)
    };
    assert_eq!(export(format!("{id}-address")).as_deref(), Some(address));
    assert_eq!(export(format!("{id}-port")), Some(port));

    let param = ssm
        .get_parameter()
        .name(format!("/{id}/host"))
        .send()
        .await
        .expect("get_parameter");
    assert_eq!(
        param.parameter().and_then(|p| p.value()),
        Some(address),
        "same-stack consumer of Endpoint.Address"
    );

    cfn.delete_stack()
        .stack_name(stack_name)
        .send()
        .await
        .expect("delete_stack");
}

#[tokio::test]
async fn create_stack_outputs_carry_real_rds_endpoint() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    cfn.create_stack()
        .stack_name("rds-endpoint-create")
        .template_body(template("rds-endpoint-create"))
        .send()
        .await
        .expect("create_stack");
    assert_real_endpoint(
        &server,
        "rds-endpoint-create",
        "rds-endpoint-create",
        "CREATE_COMPLETE",
    )
    .await;
}

#[tokio::test]
async fn execute_change_set_outputs_carry_real_rds_endpoint() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let cs = cfn
        .create_change_set()
        .stack_name("rds-endpoint-cs")
        .change_set_name("create")
        .change_set_type(ChangeSetType::Create)
        .template_body(template("rds-endpoint-cs"))
        .send()
        .await
        .expect("create_change_set");
    let cs_id = cs.id().expect("change set id");
    cfn.execute_change_set()
        .change_set_name(cs_id)
        .send()
        .await
        .expect("execute_change_set");
    if !helpers::container_cli().is_empty() {
        // ExecuteChangeSet returns while the container is still booting.
        let stack = cfn
            .describe_stacks()
            .stack_name("rds-endpoint-cs")
            .send()
            .await
            .expect("describe_stacks");
        assert_eq!(
            stack.stacks()[0].stack_status().map(|s| s.as_str()),
            Some("CREATE_IN_PROGRESS")
        );
        assert_eq!(execution_status(&cfn, cs_id).await, "EXECUTE_IN_PROGRESS");
    }
    wait_stack_terminal(&cfn, "rds-endpoint-cs").await;
    assert_eq!(execution_status(&cfn, cs_id).await, "EXECUTE_COMPLETE");
    assert_real_endpoint(
        &server,
        "rds-endpoint-cs",
        "rds-endpoint-cs",
        "CREATE_COMPLETE",
    )
    .await;
}

async fn execution_status(cfn: &aws_sdk_cloudformation::Client, cs_id: &str) -> String {
    cfn.describe_change_set()
        .change_set_name(cs_id)
        .send()
        .await
        .expect("describe_change_set")
        .execution_status()
        .expect("execution status")
        .as_str()
        .to_string()
}

/// A change set whose DB instance never starts fails the execution and rolls
/// the stack back, as CreateStack does (see
/// `create_stack_fails_when_rds_container_fails`).
#[tokio::test]
async fn execute_change_set_fails_when_rds_container_fails() {
    if helpers::container_cli().is_empty() {
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let rds = server.rds_client().await;
    let id = "rds-endpoint-cs-broken";
    let cs = cfn
        .create_change_set()
        .stack_name(id)
        .change_set_name("create")
        .change_set_type(ChangeSetType::Create)
        .template_body(template_with_version(id, "99.1"))
        .send()
        .await
        .expect("create_change_set");
    let cs_id = cs.id().expect("change set id");
    cfn.execute_change_set()
        .change_set_name(cs_id)
        .send()
        .await
        .expect("execute_change_set");

    let stack = wait_stack_terminal(&cfn, id).await;
    assert_eq!(
        stack.stack_status().map(|s| s.as_str()),
        Some("ROLLBACK_COMPLETE")
    );
    let reason = stack.stack_status_reason().unwrap_or_default();
    assert!(
        reason.contains("Db") && reason.contains("DB Instance is in state: failed"),
        "reason: {reason}"
    );
    assert_eq!(execution_status(&cfn, cs_id).await, "EXECUTE_FAILED");
    let described = rds
        .describe_db_instances()
        .db_instance_identifier(id)
        .send()
        .await;
    assert!(
        described.is_err(),
        "rolled-back instance is gone: {described:?}"
    );
}

/// An UpdateStack that adds the instance to a running stack must not reach
/// UPDATE_COMPLETE, or export the endpoint, before the instance is available.
#[tokio::test]
async fn update_stack_adding_instance_outputs_carry_real_rds_endpoint() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let id = "rds-endpoint-update";
    cfn.create_stack()
        .stack_name(id)
        .template_body(
            r#"{"Resources": {"Seed": {"Type": "AWS::SSM::Parameter",
                "Properties": {"Name": "/rds-endpoint-update/seed", "Type": "String", "Value": "x"}}}}"#,
        )
        .send()
        .await
        .expect("create_stack");
    let created = wait_stack_terminal(&cfn, id).await;
    assert_eq!(
        created.stack_status().map(|s| s.as_str()),
        Some("CREATE_COMPLETE")
    );

    cfn.update_stack()
        .stack_name(id)
        .template_body(template(id))
        .send()
        .await
        .expect("update_stack");
    assert_real_endpoint(&server, id, id, "UPDATE_COMPLETE").await;
}

/// A DB instance whose container never starts fails the resource and rolls the
/// stack back, as AWS's handler fails a `failed` instance with
/// "DB Instance is in state: failed". Postgres 99 has no image, which is just a
/// reliable way to make the container start fail (real AWS rejects the version
/// up front instead). Only meaningful with a container runtime; without one
/// the instance is metadata-only and the stack completes.
#[tokio::test]
async fn create_stack_fails_when_rds_container_fails() {
    if helpers::container_cli().is_empty() {
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let rds = server.rds_client().await;
    let id = "rds-endpoint-broken";
    cfn.create_stack()
        .stack_name(id)
        .template_body(template_with_version(id, "99.1"))
        .send()
        .await
        .expect("create_stack");

    let stack = wait_stack_terminal(&cfn, id).await;
    assert_eq!(
        stack.stack_status().map(|s| s.as_str()),
        Some("CREATE_FAILED")
    );
    let reason = stack.stack_status_reason().unwrap_or_default();
    assert!(
        reason.contains("Db") && reason.contains("DB Instance is in state: failed"),
        "reason: {reason}"
    );
    let described = rds
        .describe_db_instances()
        .db_instance_identifier(id)
        .send()
        .await;
    assert!(
        described.is_err(),
        "rolled-back instance is gone: {described:?}"
    );
}
