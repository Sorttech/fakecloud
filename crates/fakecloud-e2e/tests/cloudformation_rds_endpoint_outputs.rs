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
//! Covers both CreateStack and the change-set path `cdk deploy` uses. With no
//! container runtime the instance is `available` at insert and the endpoint
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

/// Once the stack reaches `expected_status`, the instance is available and
/// every value derived from its endpoint matches DescribeDBInstances.
async fn assert_real_endpoint_after(
    server: &TestServer,
    stack_name: &str,
    id: &str,
    expected_status: &str,
) {
    let cfn = server.cloudformation_client().await;
    let rds = server.rds_client().await;
    let ssm = server.ssm_client().await;

    let stack = wait_stack_terminal(&cfn, stack_name).await;
    assert_eq!(
        stack.stack_status().map(|s| s.as_str()),
        Some(expected_status),
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

async fn assert_real_endpoint(server: &TestServer, stack_name: &str, id: &str) {
    assert_real_endpoint_after(server, stack_name, id, "CREATE_COMPLETE").await;
}

/// A stack with nothing container-backed yet, for an update to add the DB to.
const SEED_TEMPLATE: &str = r#"{
  "Resources": {
    "Seed": {"Type": "AWS::SNS::Topic", "Properties": {"TopicName": "rds-endpoint-seed"}}
  }
}"#;

async fn create_seed_stack(cfn: &aws_sdk_cloudformation::Client, stack_name: &str) {
    cfn.create_stack()
        .stack_name(stack_name)
        .template_body(SEED_TEMPLATE)
        .send()
        .await
        .expect("create seed stack");
    let seeded = wait_stack_terminal(cfn, stack_name).await;
    assert_eq!(
        seeded.stack_status().map(|s| s.as_str()),
        Some("CREATE_COMPLETE")
    );
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
    assert_real_endpoint(&server, "rds-endpoint-create", "rds-endpoint-create").await;
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
    cfn.execute_change_set()
        .change_set_name(cs.id().expect("change set id"))
        .send()
        .await
        .expect("execute_change_set");
    assert_real_endpoint(&server, "rds-endpoint-cs", "rds-endpoint-cs").await;
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

/// While a change set is still executing, another one on the same stack is
/// refused, as AWS documents for ExecuteChangeSet's InvalidChangeSetStatus
/// ("the stack status might be UPDATE_IN_PROGRESS"). The DB instance's
/// container keeps the first execution in progress long enough to try.
#[tokio::test]
async fn execute_change_set_refuses_a_stack_in_progress() {
    if helpers::container_cli().is_empty() {
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let id = "rds-endpoint-busy";
    let mut ids = Vec::new();
    for name in ["first", "second"] {
        let cs = cfn
            .create_change_set()
            .stack_name(id)
            .change_set_name(name)
            .change_set_type(ChangeSetType::Create)
            .template_body(template(id))
            .send()
            .await
            .expect("create_change_set");
        ids.push(cs.id().expect("change set id").to_string());
    }
    cfn.execute_change_set()
        .change_set_name(&ids[0])
        .send()
        .await
        .expect("execute first change set");

    let err = cfn
        .execute_change_set()
        .change_set_name(&ids[1])
        .send()
        .await
        .expect_err("a stack in progress takes no second change set");
    assert_eq!(
        err.as_service_error().and_then(|e| e.meta().code()),
        Some("InvalidChangeSetStatus")
    );

    assert_real_endpoint(&server, id, id).await;
}

/// UpdateStack that adds a DB instance completes only once the instance is
/// available, so its Outputs, Exports and same-stack consumers carry the real
/// endpoint, as with CreateStack and ExecuteChangeSet.
#[tokio::test]
async fn update_stack_outputs_carry_real_rds_endpoint() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let id = "rds-endpoint-update";
    create_seed_stack(&cfn, id).await;
    cfn.update_stack()
        .stack_name(id)
        .template_body(template(id))
        .send()
        .await
        .expect("update_stack");
    assert_real_endpoint_after(&server, id, id, "UPDATE_COMPLETE").await;
}

/// While an update is still in progress, another UpdateStack is refused
/// rather than applied on top of it. The DB instance's container keeps the
/// first update in progress long enough to try.
#[tokio::test]
async fn update_stack_refuses_a_stack_in_progress() {
    if helpers::container_cli().is_empty() {
        return;
    }
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let id = "rds-endpoint-update-busy";
    create_seed_stack(&cfn, id).await;
    cfn.update_stack()
        .stack_name(id)
        .template_body(template(id))
        .send()
        .await
        .expect("first update_stack");

    let err = cfn
        .update_stack()
        .stack_name(id)
        .template_body(SEED_TEMPLATE)
        .send()
        .await
        .expect_err("a stack in progress takes no second update");
    assert_eq!(
        err.as_service_error().and_then(|e| e.meta().code()),
        Some("ValidationError")
    );

    assert_real_endpoint_after(&server, id, id, "UPDATE_COMPLETE").await;
}
