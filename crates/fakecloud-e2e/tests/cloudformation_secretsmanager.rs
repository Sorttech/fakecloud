//! CloudFormation provisioner for AWS::SecretsManager::Secret.

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure};
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "ApiToken": {
      "Type": "AWS::SecretsManager::Secret",
      "Properties": {
        "Name": "cfn-test-token",
        "Description": "Token for the cfn-managed app",
        "SecretString": "hunter2",
        "Tags": [
          {"Key": "Owner", "Value": "platform"}
        ]
      }
    }
  },
  "Outputs": {
    "SecretArn": {"Value": {"Ref": "ApiToken"}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_and_deletes_secrets_manager_secret() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let sm = server.secretsmanager_client().await;

    cfn.create_stack()
        .stack_name("secret-stack")
        .template_body(TEMPLATE)
        .capabilities(Capability::CapabilityIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("secret-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let arn = stack
        .outputs()
        .iter()
        .find(|o| o.output_key() == Some("SecretArn"))
        .and_then(|o| o.output_value())
        .expect("SecretArn output")
        .to_string();
    assert!(
        arn.starts_with("arn:aws:secretsmanager:"),
        "expected ARN, got {arn}"
    );

    let value = sm
        .get_secret_value()
        .secret_id(arn.clone())
        .send()
        .await
        .expect("get_secret_value");
    assert_eq!(value.secret_string(), Some("hunter2"));

    // Same ARN form CreateSecret mints: `secret:<name>-` + six random chars.
    let suffix = arn
        .split_once(":secret:cfn-test-token-")
        .map(|(_, s)| s)
        .unwrap_or_else(|| panic!("no random suffix on {arn}"));
    assert_eq!(suffix.len(), 6, "suffix of {arn}");

    // The stack secret resolves by name and by partial ARN like any other.
    let by_name = sm
        .get_secret_value()
        .secret_id("cfn-test-token")
        .send()
        .await
        .expect("get_secret_value by name");
    assert_eq!(by_name.arn(), Some(arn.as_str()));
    let partial = &arn[..arn.len() - 7];
    let by_partial = sm
        .describe_secret()
        .secret_id(partial)
        .send()
        .await
        .expect("describe_secret by partial ARN");
    assert_eq!(by_partial.arn(), Some(arn.as_str()));
    // A secret the API mutates by ARN (the name-keyed path) works too.
    sm.put_secret_value()
        .secret_id(arn.clone())
        .secret_string("rotated")
        .send()
        .await
        .expect("put_secret_value by ARN");

    cfn.delete_stack()
        .stack_name("secret-stack")
        .send()
        .await
        .expect("delete_stack");

    let after = sm.describe_secret().secret_id(arn).send().await;
    assert!(after.is_err(), "secret should be gone after stack deletion");
}
