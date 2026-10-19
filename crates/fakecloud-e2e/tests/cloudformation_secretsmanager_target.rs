//! CloudFormation provisioner for AWS::SecretsManager::SecretTargetAttachment
//! and the GenerateSecretString form of AWS::SecretsManager::Secret (BB13).
//!
//! Provisions a Secret with GenerateSecretString, an RDS instance, and a
//! SecretTargetAttachment binding the two. Asserts via the runtime Secrets
//! Manager SDK that:
//!   * GenerateSecretString produced a JSON document including the template
//!     fields and a generated password keyed by GenerateStringKey.
//!   * SecretTargetAttachment patched host/engine/dbInstanceIdentifier into
//!     the current secret string.

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure};
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "DbInstance": {
      "Type": "AWS::RDS::DBInstance",
      "Properties": {
        "DBInstanceIdentifier": "cfn-target-db",
        "DBInstanceClass": "db.t4g.micro",
        "Engine": "postgres",
        "EngineVersion": "16.0",
        "MasterUsername": "admin",
        "MasterUserPassword": "placeholder",
        "AllocatedStorage": "20",
        "Port": 5432
      }
    },
    "DbSecret": {
      "Type": "AWS::SecretsManager::Secret",
      "Properties": {
        "Name": "cfn-target-secret",
        "Description": "DB credential secret",
        "GenerateSecretString": {
          "SecretStringTemplate": "{\"username\":\"admin\"}",
          "GenerateStringKey": "password",
          "PasswordLength": 24,
          "ExcludeCharacters": "\"@/\\"
        }
      }
    },
    "DbSecretAttachment": {
      "Type": "AWS::SecretsManager::SecretTargetAttachment",
      "Properties": {
        "SecretId": {"Ref": "DbSecret"},
        "TargetId": {"Ref": "DbInstance"},
        "TargetType": "AWS::RDS::DBInstance"
      }
    }
  },
  "Outputs": {
    "SecretArn": {"Value": {"Ref": "DbSecret"}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_secret_target_attachment_with_generated_string() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let sm = aws_sdk_secretsmanager::Client::new(&server.aws_config().await);

    cfn.create_stack()
        .stack_name("sm-target-stack")
        .template_body(TEMPLATE)
        .capabilities(Capability::CapabilityIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    // CreateStack provisions a container-backed DB instance in the
    // background, as AWS does, so wait for the stack to settle.
    let stack = helpers::wait_until(std::time::Duration::from_secs(300), || async {
        let out = cfn
            .describe_stacks()
            .stack_name("sm-target-stack")
            .send()
            .await
            .ok()?;
        let s = out.stacks().first()?.clone();
        (!s.stack_status()?.as_str().ends_with("IN_PROGRESS")).then_some(s)
    })
    .await
    .expect("stack reached a terminal status");
    assert_eq!(
        stack.stack_status().unwrap().as_str(),
        "CREATE_COMPLETE",
        "stack status reason: {:?}",
        stack.stack_status_reason()
    );

    let secret_arn = stack
        .outputs()
        .iter()
        .find(|o| o.output_key() == Some("SecretArn"))
        .and_then(|o| o.output_value())
        .expect("SecretArn output")
        .to_string();

    let value = sm
        .get_secret_value()
        .secret_id(&secret_arn)
        .send()
        .await
        .expect("get_secret_value");
    let body: serde_json::Value =
        serde_json::from_str(value.secret_string().expect("secret_string"))
            .expect("secret string is JSON");
    let obj = body.as_object().expect("secret JSON object");

    // Template field carried through.
    assert_eq!(obj.get("username").and_then(|v| v.as_str()), Some("admin"));

    // Generated password key present, length matches PasswordLength, no
    // excluded characters.
    let password = obj
        .get("password")
        .and_then(|v| v.as_str())
        .expect("generated password");
    assert_eq!(password.len(), 24, "PasswordLength honoured");
    for ch in ['"', '@', '/', '\\'] {
        assert!(
            !password.contains(ch),
            "ExcludeCharacters honoured: {ch} found in {password}"
        );
    }

    // SecretTargetAttachment patched RDS connection details.
    assert_eq!(
        obj.get("engine").and_then(|v| v.as_str()),
        Some("postgres"),
        "TargetAttachment set engine"
    );
    assert_eq!(
        obj.get("host").and_then(|v| v.as_str()),
        Some("cfn-target-db"),
        "TargetAttachment set host to TargetId"
    );
    assert_eq!(
        obj.get("dbInstanceIdentifier").and_then(|v| v.as_str()),
        Some("cfn-target-db"),
    );

    cfn.delete_stack()
        .stack_name("sm-target-stack")
        .send()
        .await
        .expect("delete_stack");
}
