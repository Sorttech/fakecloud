//! CloudFormation provisioner for AWS::Route53::HostedZone, RecordSet, and HealthCheck.

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure};
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Zone": {
      "Type": "AWS::Route53::HostedZone",
      "Properties": {
        "Name": "example.com.",
        "HostedZoneConfig": {"Comment": "managed by cfn"}
      }
    },
    "ApiRecord": {
      "Type": "AWS::Route53::RecordSet",
      "Properties": {
        "HostedZoneId": {"Ref": "Zone"},
        "Name": "api.example.com.",
        "Type": "A",
        "TTL": "300",
        "ResourceRecords": ["10.0.0.1", "10.0.0.2"]
      }
    },
    "HealthCheck": {
      "Type": "AWS::Route53::HealthCheck",
      "Properties": {
        "HealthCheckConfig": {
          "Type": "HTTP",
          "FullyQualifiedDomainName": "api.example.com",
          "Port": 80,
          "ResourcePath": "/health",
          "RequestInterval": 30,
          "FailureThreshold": 3
        }
      }
    }
  },
  "Outputs": {
    "ZoneId": {"Value": {"Ref": "Zone"}},
    "RecordPhysicalId": {"Value": {"Ref": "ApiRecord"}},
    "HealthCheckId": {"Value": {"Ref": "HealthCheck"}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_route53_resources() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let r53 = aws_sdk_route53::Client::new(&server.aws_config().await);

    cfn.create_stack()
        .stack_name("r53-stack")
        .template_body(TEMPLATE)
        .capabilities(Capability::CapabilityIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("r53-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let outputs: std::collections::HashMap<&str, &str> = stack
        .outputs()
        .iter()
        .filter_map(|o| Some((o.output_key()?, o.output_value()?)))
        .collect();

    let zone_id = outputs.get("ZoneId").expect("ZoneId output");
    let record_pid = outputs.get("RecordPhysicalId").expect("RecordPhysicalId");
    let health_id = outputs.get("HealthCheckId").expect("HealthCheckId");

    assert!(zone_id.starts_with('Z'), "zone id format: {zone_id}");
    assert!(
        record_pid.contains("api.example.com.") && record_pid.contains("|A"),
        "record physical id format: {record_pid}"
    );
    assert!(!health_id.is_empty());

    // Verify hosted zone via SDK.
    let zone = r53
        .get_hosted_zone()
        .id(*zone_id)
        .send()
        .await
        .expect("get_hosted_zone");
    let hz = zone.hosted_zone().expect("hosted zone");
    assert_eq!(hz.name(), "example.com.");

    // Verify record set landed in the zone.
    let records = r53
        .list_resource_record_sets()
        .hosted_zone_id(*zone_id)
        .send()
        .await
        .expect("list_resource_record_sets");
    let api_record = records
        .resource_record_sets()
        .iter()
        .find(|r| r.name() == "api.example.com." && r.r#type().as_str() == "A")
        .expect("api A record");
    assert_eq!(api_record.ttl(), Some(300));
    let values: Vec<&str> = api_record
        .resource_records()
        .iter()
        .map(|rr| rr.value())
        .collect();
    assert!(values.contains(&"10.0.0.1"));
    assert!(values.contains(&"10.0.0.2"));

    // Verify health check.
    let hc = r53
        .get_health_check()
        .health_check_id(*health_id)
        .send()
        .await
        .expect("get_health_check");
    let cfg = hc
        .health_check()
        .and_then(|h| h.health_check_config())
        .expect("health check config");
    assert_eq!(cfg.r#type().as_str(), "HTTP");
    assert_eq!(cfg.fully_qualified_domain_name(), Some("api.example.com"));
    assert_eq!(cfg.port(), Some(80));

    // Tear down.
    cfn.delete_stack()
        .stack_name("r53-stack")
        .send()
        .await
        .expect("delete_stack");

    let zone_after = r53.get_hosted_zone().id(*zone_id).send().await;
    assert!(zone_after.is_err(), "hosted zone should be gone");

    let hc_after = r53
        .get_health_check()
        .health_check_id(*health_id)
        .send()
        .await;
    assert!(hc_after.is_err(), "health check should be gone");
}

const DNSSEC_TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Zone": {
      "Type": "AWS::Route53::HostedZone",
      "Properties": {"Name": "dnssec.example.com."}
    },
    "Ksk": {
      "Type": "AWS::Route53::KeySigningKey",
      "Properties": {
        "HostedZoneId": {"Ref": "Zone"},
        "Name": "primary-ksk",
        "KeyManagementServiceArn": "arn:aws:kms:us-east-1:123456789012:key/dummy",
        "Status": "ACTIVE"
      }
    },
    "EnableDnssec": {
      "Type": "AWS::Route53::DNSSEC",
      "DependsOn": "Ksk",
      "Properties": {
        "HostedZoneId": {"Ref": "Zone"}
      }
    }
  },
  "Outputs": {
    "ZoneId": {"Value": {"Ref": "Zone"}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_route53_dnssec_and_ksk() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let r53 = aws_sdk_route53::Client::new(&server.aws_config().await);

    cfn.create_stack()
        .stack_name("r53-dnssec-stack")
        .template_body(DNSSEC_TEMPLATE)
        .send()
        .await
        .expect("create_stack");

    let described = cfn
        .describe_stacks()
        .stack_name("r53-dnssec-stack")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().unwrap();
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let zone_id = stack
        .outputs()
        .iter()
        .find(|o| o.output_key() == Some("ZoneId"))
        .and_then(|o| o.output_value())
        .map(|s| s.to_string())
        .expect("ZoneId output");

    let dnssec = r53
        .get_dnssec()
        .hosted_zone_id(&zone_id)
        .send()
        .await
        .expect("get_dnssec");
    assert_eq!(
        dnssec.status().and_then(|s| s.serve_signature()),
        Some("SIGNING"),
    );
    let ksks = dnssec.key_signing_keys();
    assert!(
        ksks.iter().any(|k| k.name() == Some("primary-ksk")),
        "KSK should be present: {ksks:?}"
    );

    cfn.delete_stack()
        .stack_name("r53-dnssec-stack")
        .send()
        .await
        .expect("delete_stack");

    // Hosted zone gone -> get_dnssec should error.
    let after = r53.get_dnssec().hosted_zone_id(&zone_id).send().await;
    assert!(
        after.is_err(),
        "DNSSEC config should be gone after stack deletion"
    );
}

/// A stack owns its Route 53 resources in the stack's account (#2633): a
/// second account's stack creates a zone + records visible through that
/// account's Route 53 API and not through the default account's.
#[tokio::test]
async fn cfn_route53_resources_belong_to_the_stacks_account() {
    use aws_credential_types::Credentials;

    const ACCOUNT_B: &str = "222222222222";
    let server = TestServer::start_with_env(&[("FAKECLOUD_IAM", "soft")]).await;
    let (akid, secret) = server.create_admin(ACCOUNT_B, "admin-b").await;
    let cfg_b = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(akid, secret, None, None, "r53-acct-b"))
        .load()
        .await;
    let cfn_b = aws_sdk_cloudformation::Client::new(&cfg_b);
    let r53_b = aws_sdk_route53::Client::new(&cfg_b);
    let r53_default = aws_sdk_route53::Client::new(&server.aws_config().await);

    cfn_b
        .create_stack()
        .stack_name("r53-acct-b")
        .template_body(TEMPLATE)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack in account B");
    let described = cfn_b
        .describe_stacks()
        .stack_name("r53-acct-b")
        .send()
        .await
        .expect("describe_stacks");
    let stack = described.stacks().first().expect("stack present");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");
    assert!(
        stack
            .stack_id()
            .unwrap()
            .contains(&format!(":{ACCOUNT_B}:")),
        "stack lives in account B: {:?}",
        stack.stack_id()
    );
    let outputs: std::collections::HashMap<&str, &str> = stack
        .outputs()
        .iter()
        .filter_map(|o| Some((o.output_key()?, o.output_value()?)))
        .collect();
    let zone_id = outputs["ZoneId"];
    let health_id = outputs["HealthCheckId"];

    // Account B sees the zone, its records and the health check...
    let zone = r53_b
        .get_hosted_zone()
        .id(zone_id)
        .send()
        .await
        .expect("account B reads its zone");
    assert_eq!(zone.hosted_zone().unwrap().name(), "example.com.");
    let listed = r53_b.list_hosted_zones().send().await.unwrap();
    assert!(listed
        .hosted_zones()
        .iter()
        .any(|z| z.id().ends_with(zone_id)));
    let records = r53_b
        .list_resource_record_sets()
        .hosted_zone_id(zone_id)
        .send()
        .await
        .expect("account B lists its records");
    let mut types: Vec<&str> = records
        .resource_record_sets()
        .iter()
        .map(|r| r.r#type().as_str())
        .collect();
    types.sort();
    // The stack's A record plus the default SOA + NS records every zone
    // carries, exactly as CreateHostedZone stores them.
    assert_eq!(types, ["A", "NS", "SOA"]);
    r53_b
        .get_health_check()
        .health_check_id(health_id)
        .send()
        .await
        .expect("account B reads its health check");

    // ...the default account does not.
    let err = r53_default
        .get_hosted_zone()
        .id(zone_id)
        .send()
        .await
        .expect_err("default account must not see account B's zone");
    assert_eq!(
        err.as_service_error().and_then(|e| e.meta().code()),
        Some("NoSuchHostedZone")
    );
    let err = r53_default
        .list_resource_record_sets()
        .hosted_zone_id(zone_id)
        .send()
        .await
        .expect_err("default account must not list account B's records");
    assert_eq!(
        err.as_service_error().and_then(|e| e.meta().code()),
        Some("NoSuchHostedZone")
    );
    let listed = r53_default.list_hosted_zones().send().await.unwrap();
    assert!(listed.hosted_zones().is_empty(), "{listed:?}");
    assert!(r53_default
        .get_health_check()
        .health_check_id(health_id)
        .send()
        .await
        .is_err());

    // DNS resolution spans accounts, so the stack's record still resolves.
    let resolved: serde_json::Value = reqwest::get(format!(
        "{}/_fakecloud/dns/resolve?name=api.example.com&type=A",
        server.endpoint()
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(resolved["status"], "ANSWERED", "{resolved}");

    // Deleting the stack removes the zone from account B.
    cfn_b
        .delete_stack()
        .stack_name("r53-acct-b")
        .send()
        .await
        .expect("delete_stack");
    assert!(r53_b.get_hosted_zone().id(zone_id).send().await.is_err());
}
