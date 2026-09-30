//! IAM PassRole trust-policy enforcement across services that accept a
//! role ARN (Lambda CreateFunction, ECS RegisterTaskDefinition / RunTask).
//!
//! AWS validates two things when a service is given a role:
//!   1. caller has `iam:PassRole` on the role (identity-policy half),
//!   2. role's `AssumeRolePolicyDocument` allows the service principal
//!      (trust-policy half).
//!
//! These tests cover the trust-policy half — the service-side check
//! that real AWS enforces unconditionally regardless of the caller's
//! identity policy.

mod helpers;

use aws_sdk_ecs::types::ContainerDefinition;
use helpers::TestServer;

const LAMBDA_TRUST: &str = r#"{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": {"Service": "lambda.amazonaws.com"},
    "Action": "sts:AssumeRole"
  }]
}"#;

const ECS_TASKS_TRUST: &str = r#"{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": {"Service": "ecs-tasks.amazonaws.com"},
    "Action": "sts:AssumeRole"
  }]
}"#;

const EC2_TRUST: &str = r#"{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": {"Service": "ec2.amazonaws.com"},
    "Action": "sts:AssumeRole"
  }]
}"#;

async fn create_role(iam: &aws_sdk_iam::Client, name: &str, trust: &str) -> String {
    iam.create_role()
        .role_name(name)
        .assume_role_policy_document(trust)
        .send()
        .await
        .unwrap()
        .role()
        .unwrap()
        .arn()
        .to_string()
}

#[tokio::test]
async fn lambda_create_function_rejects_role_without_lambda_trust() {
    let server = TestServer::start().await;
    let iam = server.iam_client().await;
    let lambda = server.lambda_client().await;

    let bad_role = create_role(&iam, "ec2-only-role", EC2_TRUST).await;

    let zip = aws_sdk_lambda::primitives::Blob::new(minimal_zip());
    let err = lambda
        .create_function()
        .function_name("rejects-bad-role")
        .runtime(aws_sdk_lambda::types::Runtime::Provided)
        .role(bad_role.clone())
        .handler("index.handler")
        .code(
            aws_sdk_lambda::types::FunctionCode::builder()
                .zip_file(zip)
                .build(),
        )
        .send()
        .await
        .expect_err(
            "CreateFunction should fail when role's trust policy excludes lambda.amazonaws.com",
        );

    let msg = format!("{err:?}");
    assert!(
        msg.contains("trust policy")
            || msg.contains("InvalidParameterValueException")
            || msg.contains("lambda.amazonaws.com"),
        "expected PassRole trust-policy rejection, got: {msg}"
    );
}

#[tokio::test]
async fn lambda_create_function_accepts_role_with_lambda_trust() {
    let server = TestServer::start().await;
    let iam = server.iam_client().await;
    let lambda = server.lambda_client().await;

    let good_role = create_role(&iam, "lambda-exec", LAMBDA_TRUST).await;

    let zip = aws_sdk_lambda::primitives::Blob::new(minimal_zip());
    let resp = lambda
        .create_function()
        .function_name("trust-policy-ok")
        .runtime(aws_sdk_lambda::types::Runtime::Provided)
        .role(good_role)
        .handler("index.handler")
        .code(
            aws_sdk_lambda::types::FunctionCode::builder()
                .zip_file(zip)
                .build(),
        )
        .send()
        .await
        .expect("CreateFunction should succeed when role trusts lambda.amazonaws.com");
    assert_eq!(resp.function_name(), Some("trust-policy-ok"));
}

#[tokio::test]
async fn ecs_register_task_definition_rejects_role_without_ecs_tasks_trust() {
    let server = TestServer::start().await;
    let iam = server.iam_client().await;
    let ecs = server.ecs_client().await;

    let bad_role = create_role(&iam, "lambda-only-role", LAMBDA_TRUST).await;

    let cd = ContainerDefinition::builder()
        .name("app")
        .image("public.ecr.aws/docker/library/alpine:3.19")
        .build();

    let err = ecs
        .register_task_definition()
        .family("rejects-bad-task-role")
        .container_definitions(cd)
        .task_role_arn(bad_role.clone())
        .send()
        .await
        .expect_err("RegisterTaskDefinition should fail when taskRoleArn doesn't trust ecs-tasks.amazonaws.com");

    // ECS answers a role it cannot assume with ClientException naming it.
    let err = err.into_service_error();
    assert!(err.is_client_exception(), "{err:?}");
    assert!(
        err.meta().message().is_some_and(
            |m| m.starts_with(&format!("ECS was unable to assume the role '{bad_role}'"))
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn ecs_register_task_definition_accepts_role_with_ecs_tasks_trust() {
    let server = TestServer::start().await;
    let iam = server.iam_client().await;
    let ecs = server.ecs_client().await;

    let good_role = create_role(&iam, "ecs-task", ECS_TASKS_TRUST).await;
    let exec_role = create_role(&iam, "ecs-exec", ECS_TASKS_TRUST).await;

    let cd = ContainerDefinition::builder()
        .name("app")
        .image("public.ecr.aws/docker/library/alpine:3.19")
        .build();

    let resp = ecs
        .register_task_definition()
        .family("good-roles")
        .container_definitions(cd)
        .task_role_arn(good_role)
        .execution_role_arn(exec_role)
        .send()
        .await
        .expect("RegisterTaskDefinition should accept ecs-tasks.amazonaws.com-trusting roles");
    assert_eq!(resp.task_definition().unwrap().family(), Some("good-roles"));
}

/// Smallest possible zip with a single empty file. Lambda CreateFunction
/// won't actually invoke the function in this test — we only need the
/// service to accept the upload and complete the create.
fn minimal_zip() -> Vec<u8> {
    use std::io::Write;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
        zip.start_file("index.sh", opts).unwrap();
        zip.write_all(b"#!/bin/sh\necho hi\n").unwrap();
        zip.finish().unwrap();
    }
    buf
}
