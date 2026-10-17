mod helpers;

use aws_sdk_ec2::types::Filter;
use aws_sdk_route53resolver::types::{IpAddressRequest, ResolverEndpointDirection, RuleTypeOption};
use helpers::TestServer;

/// An endpoint created without `Protocols` and a SYSTEM rule (which carries no
/// `TargetIps`) both survive a restart; the endpoint reports AWS's implicit
/// `Do53` before and after.
#[tokio::test]
async fn persistence_round_trip_endpoint_without_protocols_and_system_rule() {
    let tmp = tempfile::tempdir().unwrap();
    let mut server = TestServer::start_persistent(tmp.path()).await;
    let ec2 = server.ec2_client().await;
    let r53r = aws_sdk_route53resolver::Client::new(&server.aws_config().await);

    let subnets = ec2
        .describe_subnets()
        .filters(
            Filter::builder()
                .name("default-for-az")
                .values("true")
                .build(),
        )
        .send()
        .await
        .unwrap()
        .subnets
        .unwrap();
    assert!(subnets.len() >= 2, "default VPC needs two subnets");
    let sg = ec2
        .describe_security_groups()
        .filters(
            Filter::builder()
                .name("group-name")
                .values("default")
                .build(),
        )
        .filters(
            Filter::builder()
                .name("vpc-id")
                .values(subnets[0].vpc_id().unwrap())
                .build(),
        )
        .send()
        .await
        .unwrap()
        .security_groups
        .unwrap()
        .remove(0)
        .group_id
        .unwrap();

    let mut create = r53r
        .create_resolver_endpoint()
        .creator_request_id("persist-ep")
        .direction(ResolverEndpointDirection::Inbound)
        .security_group_ids(&sg);
    for subnet in &subnets[..2] {
        create = create.ip_addresses(
            IpAddressRequest::builder()
                .subnet_id(subnet.subnet_id().unwrap())
                .build()
                .unwrap(),
        );
    }
    let endpoint = create.send().await.unwrap().resolver_endpoint.unwrap();
    assert_eq!(endpoint.protocols().len(), 1);
    assert_eq!(endpoint.protocols()[0].as_str(), "Do53");
    let endpoint_id = endpoint.id.unwrap();

    let rule_id = r53r
        .create_resolver_rule()
        .creator_request_id("persist-rule")
        .rule_type(RuleTypeOption::System)
        .domain_name("internal.example.com")
        .send()
        .await
        .unwrap()
        .resolver_rule
        .unwrap()
        .id
        .unwrap();

    server.restart().await;
    let r53r = aws_sdk_route53resolver::Client::new(&server.aws_config().await);

    let endpoint = r53r
        .get_resolver_endpoint()
        .resolver_endpoint_id(&endpoint_id)
        .send()
        .await
        .unwrap()
        .resolver_endpoint
        .unwrap();
    assert_eq!(endpoint.protocols().len(), 1);
    assert_eq!(endpoint.protocols()[0].as_str(), "Do53");

    let rule = r53r
        .get_resolver_rule()
        .resolver_rule_id(&rule_id)
        .send()
        .await
        .unwrap()
        .resolver_rule
        .unwrap();
    assert!(rule.target_ips().is_empty());
}
