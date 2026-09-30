//! CloudFormation provisioner for AWS::Events::Connection + ApiDestination + Archive.

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure};
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Conn": {
      "Type": "AWS::Events::Connection",
      "Properties": {
        "Name": "cfn-conn",
        "AuthorizationType": "API_KEY",
        "AuthParameters": {
          "ApiKeyAuthParameters": {
            "ApiKeyName": "X-API-Key",
            "ApiKeyValue": "secret-shhh"
          }
        }
      }
    },
    "Dest": {
      "Type": "AWS::Events::ApiDestination",
      "Properties": {
        "Name": "cfn-dest",
        "ConnectionArn": {"Fn::GetAtt": ["Conn", "Arn"]},
        "InvocationEndpoint": "https://example.com/webhook",
        "HttpMethod": "POST",
        "InvocationRateLimitPerSecond": 10
      }
    },
    "Arch": {
      "Type": "AWS::Events::Archive",
      "Properties": {
        "ArchiveName": "cfn-archive",
        "SourceArn": "arn:aws:events:us-east-1:123456789012:event-bus/default",
        "RetentionDays": 30
      }
    }
  },
  "Outputs": {
    "ConnArn": {"Value": {"Fn::GetAtt": ["Conn", "Arn"]}},
    "ConnSecretArn": {"Value": {"Fn::GetAtt": ["Conn", "SecretArn"]}},
    "DestArn": {"Value": {"Fn::GetAtt": ["Dest", "Arn"]}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_events_connection_apidest_archive() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let eb = server.eventbridge_client().await;

    cfn.create_stack()
        .stack_name("events-stack")
        .template_body(TEMPLATE)
        .capabilities(Capability::CapabilityIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("events-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let conn = eb
        .describe_connection()
        .name("cfn-conn")
        .send()
        .await
        .expect("describe_connection");
    assert_eq!(conn.name(), Some("cfn-conn"));
    assert_eq!(
        conn.authorization_type().map(|a| a.as_str()),
        Some("API_KEY")
    );

    let output = |key: &str| -> String {
        stack
            .outputs()
            .iter()
            .find(|o| o.output_key() == Some(key))
            .and_then(|o| o.output_value())
            .unwrap_or_else(|| panic!("stack output {key}"))
            .to_string()
    };

    // The stack reports the same ARNs the EventBridge API does, and the
    // connection's secret is named after the connection and its ARN's UUID,
    // exactly like a connection created through CreateConnection.
    let conn_arn = conn.connection_arn().expect("connection ARN");
    assert_eq!(output("ConnArn"), conn_arn);
    let secret_arn = conn.secret_arn().expect("secret ARN");
    assert_eq!(output("ConnSecretArn"), secret_arn);
    let conn_uuid = conn_arn
        .strip_prefix("arn:aws:events:us-east-1:123456789012:connection/cfn-conn/")
        .expect("connection ARN shape");
    assert!(uuid::Uuid::parse_str(conn_uuid).is_ok() && conn_uuid.len() == 36);
    assert_eq!(
        secret_arn,
        format!(
            "arn:aws:secretsmanager:us-east-1:123456789012:secret:events!connection/cfn-conn/{conn_uuid}"
        )
    );

    // Same shape as the API path.
    let api_conn = eb
        .create_connection()
        .name("api-conn")
        .authorization_type(aws_sdk_eventbridge::types::ConnectionAuthorizationType::ApiKey)
        .auth_parameters(
            aws_sdk_eventbridge::types::CreateConnectionAuthRequestParameters::builder()
                .api_key_auth_parameters(
                    aws_sdk_eventbridge::types::CreateConnectionApiKeyAuthRequestParameters::builder()
                        .api_key_name("X-API-Key")
                        .api_key_value("v")
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .expect("create_connection");
    let api_arn = api_conn.connection_arn().unwrap();
    let api_uuid = api_arn
        .strip_prefix("arn:aws:events:us-east-1:123456789012:connection/api-conn/")
        .expect("API connection ARN shape");
    let api_secret = eb
        .describe_connection()
        .name("api-conn")
        .send()
        .await
        .expect("describe api-conn")
        .secret_arn()
        .unwrap()
        .to_string();
    assert_eq!(
        api_secret,
        format!(
            "arn:aws:secretsmanager:us-east-1:123456789012:secret:events!connection/api-conn/{api_uuid}"
        )
    );

    let dest = eb
        .describe_api_destination()
        .name("cfn-dest")
        .send()
        .await
        .expect("describe_api_destination");
    assert_eq!(dest.name(), Some("cfn-dest"));
    assert_eq!(
        dest.invocation_endpoint(),
        Some("https://example.com/webhook")
    );
    assert_eq!(dest.invocation_rate_limit_per_second(), Some(10));
    let dest_arn = dest.api_destination_arn().expect("api destination ARN");
    assert_eq!(output("DestArn"), dest_arn);
    let dest_uuid = dest_arn
        .strip_prefix("arn:aws:events:us-east-1:123456789012:api-destination/cfn-dest/")
        .expect("api destination ARN shape");
    assert!(uuid::Uuid::parse_str(dest_uuid).is_ok() && dest_uuid.len() == 36);
    assert_eq!(dest.connection_arn(), Some(conn_arn));

    let archive = eb
        .describe_archive()
        .archive_name("cfn-archive")
        .send()
        .await
        .expect("describe_archive");
    assert_eq!(archive.archive_name(), Some("cfn-archive"));
    assert_eq!(archive.retention_days(), Some(30));

    cfn.delete_stack()
        .stack_name("events-stack")
        .send()
        .await
        .expect("delete_stack");

    let after = eb.describe_connection().name("cfn-conn").send().await;
    assert!(
        after.is_err(),
        "connection should be gone after stack deletion"
    );
}
