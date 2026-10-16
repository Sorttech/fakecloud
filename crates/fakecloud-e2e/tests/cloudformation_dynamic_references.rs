//! CloudFormation dynamic references (`{{resolve:ssm:...}}`,
//! `{{resolve:ssm-secure:...}}`, `{{resolve:secretsmanager:...}}`) in resource
//! properties resolve to the stored value when the resource is provisioned.

mod helpers;

use aws_sdk_cloudformation::types::ChangeSetType;
use aws_sdk_ssm::types::ParameterType;
use helpers::{wait_until, TestServer};
use std::time::Duration;

/// Seed one plain and one secure SSM parameter and a JSON secret; return the
/// secret's full ARN.
async fn seed(server: &TestServer) -> String {
    let ssm = server.ssm_client().await;
    ssm.put_parameter()
        .name("/dynref/plain")
        .value("plain-v1")
        .r#type(ParameterType::String)
        .send()
        .await
        .unwrap();
    ssm.put_parameter()
        .name("/dynref/plain")
        .value("plain-v2")
        .r#type(ParameterType::String)
        .overwrite(true)
        .send()
        .await
        .unwrap();
    ssm.put_parameter()
        .name("dynref-secure")
        .value("secure-test-value")
        .r#type(ParameterType::SecureString)
        .send()
        .await
        .unwrap();
    let sm = server.secretsmanager_client().await;
    sm.create_secret()
        .name("dynref-db")
        .secret_string(r#"{"username":"app_user","password":"test-password"}"#)
        .send()
        .await
        .unwrap()
        .arn()
        .unwrap()
        .to_string()
}

/// A template whose SSM parameters' values are dynamic references, in every
/// documented shape: bare, versioned, embedded (two in one string), the whole
/// SecretString, a JSON key by name, and the CDK `fromGeneratedSecret` shape
/// (full ARN with trailing empty version segments).
fn template(secret_arn: &str) -> String {
    serde_json::json!({
        "Resources": {
            "Latest": {"Type": "AWS::SSM::Parameter", "Properties": {
                "Name": "/out/latest", "Type": "String",
                "Value": "{{resolve:ssm:/dynref/plain}}"}},
            "Versioned": {"Type": "AWS::SSM::Parameter", "Properties": {
                "Name": "/out/versioned", "Type": "String",
                "Value": "{{resolve:ssm:/dynref/plain:1}}"}},
            "Secure": {"Type": "AWS::SSM::Parameter", "Properties": {
                "Name": "/out/secure", "Type": "String",
                "Value": "{{resolve:ssm-secure:dynref-secure:1}}"}},
            "Embedded": {"Type": "AWS::SSM::Parameter", "Properties": {
                "Name": "/out/embedded", "Type": "String",
                "Value": "postgres://{{resolve:secretsmanager:dynref-db:SecretString:username}}@{{resolve:ssm:/dynref/plain}}/db"}},
            "Whole": {"Type": "AWS::SSM::Parameter", "Properties": {
                "Name": "/out/whole", "Type": "String",
                "Value": "{{resolve:secretsmanager:dynref-db}}"}},
            "Cdk": {"Type": "AWS::SSM::Parameter", "Properties": {
                "Name": "/out/cdk", "Type": "String",
                "Value": {"Fn::Join": ["", [
                    "{{resolve:secretsmanager:", secret_arn, ":SecretString:password::}}"
                ]]}}}
        }
    })
    .to_string()
}

async fn stack_status(
    cf: &aws_sdk_cloudformation::Client,
    name: &str,
    want: &[&str],
) -> (String, Option<String>) {
    wait_until(Duration::from_secs(30), || async {
        let out = cf.describe_stacks().stack_name(name).send().await.ok()?;
        let stack = out.stacks().first()?;
        let status = stack.stack_status()?.as_str().to_string();
        want.contains(&status.as_str())
            .then(|| (status, stack.stack_status_reason().map(str::to_string)))
    })
    .await
    .unwrap_or_else(|| panic!("stack {name} never reached {want:?}"))
}

async fn assert_resolved(server: &TestServer) {
    let ssm = server.ssm_client().await;
    for (name, want) in [
        ("/out/latest", "plain-v2"),
        ("/out/versioned", "plain-v1"),
        ("/out/secure", "secure-test-value"),
        ("/out/embedded", "postgres://app_user@plain-v2/db"),
        (
            "/out/whole",
            r#"{"username":"app_user","password":"test-password"}"#,
        ),
        ("/out/cdk", "test-password"),
    ] {
        let got = ssm.get_parameter().name(name).send().await.unwrap();
        assert_eq!(got.parameter().unwrap().value(), Some(want), "{name}");
    }
}

#[tokio::test]
async fn create_stack_resolves_dynamic_references() {
    let server = TestServer::start().await;
    let arn = seed(&server).await;
    let cf = server.cloudformation_client().await;
    let body = template(&arn);
    cf.create_stack()
        .stack_name("dynref")
        .template_body(&body)
        .send()
        .await
        .unwrap();
    let (status, reason) = stack_status(&cf, "dynref", &["CREATE_COMPLETE", "CREATE_FAILED"]).await;
    assert_eq!(status, "CREATE_COMPLETE", "{reason:?}");
    assert_resolved(&server).await;

    // GetTemplate returns the template as submitted: the literal references,
    // never the resolved secret values.
    let got = cf.get_template().stack_name("dynref").send().await.unwrap();
    let returned = got.template_body().unwrap();
    assert!(returned.contains("{{resolve:secretsmanager:dynref-db}}"));
    assert!(!returned.contains("test-password"));
}

#[tokio::test]
async fn execute_change_set_resolves_dynamic_references() {
    let server = TestServer::start().await;
    let arn = seed(&server).await;
    let cf = server.cloudformation_client().await;
    cf.create_change_set()
        .stack_name("dynref-cs")
        .change_set_name("create")
        .change_set_type(ChangeSetType::Create)
        .template_body(template(&arn))
        .send()
        .await
        .unwrap();
    cf.execute_change_set()
        .stack_name("dynref-cs")
        .change_set_name("create")
        .send()
        .await
        .unwrap();
    let (status, reason) = stack_status(
        &cf,
        "dynref-cs",
        &["CREATE_COMPLETE", "ROLLBACK_COMPLETE", "CREATE_FAILED"],
    )
    .await;
    assert_eq!(status, "CREATE_COMPLETE", "{reason:?}");
    assert_resolved(&server).await;
}

#[tokio::test]
async fn update_stack_resolves_dynamic_references_in_changed_resource() {
    let server = TestServer::start().await;
    seed(&server).await;
    let cf = server.cloudformation_client().await;
    let with_value = |value: &str| {
        serde_json::json!({"Resources": {"P": {"Type": "AWS::SSM::Parameter", "Properties": {
            "Name": "/out/updated", "Type": "String", "Value": value}}}})
        .to_string()
    };
    cf.create_stack()
        .stack_name("dynref-up")
        .template_body(with_value("before"))
        .send()
        .await
        .unwrap();
    stack_status(&cf, "dynref-up", &["CREATE_COMPLETE"]).await;
    cf.update_stack()
        .stack_name("dynref-up")
        .template_body(with_value(
            "{{resolve:secretsmanager:dynref-db:SecretString:password}}",
        ))
        .send()
        .await
        .unwrap();
    let (status, reason) = stack_status(
        &cf,
        "dynref-up",
        &["UPDATE_COMPLETE", "UPDATE_ROLLBACK_COMPLETE"],
    )
    .await;
    assert_eq!(status, "UPDATE_COMPLETE", "{reason:?}");
    let ssm = server.ssm_client().await;
    let got = ssm
        .get_parameter()
        .name("/out/updated")
        .send()
        .await
        .unwrap();
    assert_eq!(got.parameter().unwrap().value(), Some("test-password"));
}

#[tokio::test]
async fn create_stack_fails_on_missing_secret() {
    let server = TestServer::start().await;
    let cf = server.cloudformation_client().await;
    let body =
        serde_json::json!({"Resources": {"P": {"Type": "AWS::SSM::Parameter", "Properties": {
        "Name": "/out/missing", "Type": "String",
        "Value": "{{resolve:secretsmanager:no-such-secret:SecretString:password}}"}}}})
        .to_string();
    cf.create_stack()
        .stack_name("dynref-missing")
        .template_body(body)
        .send()
        .await
        .unwrap();
    let (status, reason) =
        stack_status(&cf, "dynref-missing", &["CREATE_COMPLETE", "CREATE_FAILED"]).await;
    assert_eq!(status, "CREATE_FAILED");
    let reason = reason.unwrap_or_default();
    assert!(
        reason.contains(
            "Secrets Manager can't find the specified secret. (Service: AWSSecretsManager; \
             Status Code: 400; Error Code: ResourceNotFoundException;"
        ),
        "{reason}"
    );
    let ssm = server.ssm_client().await;
    assert!(ssm
        .get_parameter()
        .name("/out/missing")
        .send()
        .await
        .is_err());
}

#[tokio::test]
async fn create_stack_fails_on_missing_ssm_parameter() {
    let server = TestServer::start().await;
    let cf = server.cloudformation_client().await;
    let body =
        serde_json::json!({"Resources": {"P": {"Type": "AWS::SSM::Parameter", "Properties": {
        "Name": "/out/missing", "Type": "String",
        "Value": "{{resolve:ssm:/no/such/param}}"}}}})
        .to_string();
    cf.create_stack()
        .stack_name("dynref-missing-ssm")
        .template_body(body)
        .send()
        .await
        .unwrap();
    let (status, reason) = stack_status(
        &cf,
        "dynref-missing-ssm",
        &["CREATE_COMPLETE", "CREATE_FAILED"],
    )
    .await;
    assert_eq!(status, "CREATE_FAILED");
    let reason = reason.unwrap_or_default();
    assert!(
        reason.contains(
            "Unable to fetch parameters [/no/such/param] from parameter store for this account."
        ),
        "{reason}"
    );
}
