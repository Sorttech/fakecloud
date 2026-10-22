//! CloudFormation provisioner for AWS::ElastiCache::CacheCluster +
//! ReplicationGroup (BB24). Provisions both, asserts via the runtime
//! ElastiCache SDK. Skips Docker container spawn — control-plane only.

mod helpers;

use aws_sdk_cloudformation::types::{Capability, OnFailure};
use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "AWSTemplateFormatVersion": "2010-09-09",
  "Resources": {
    "Cluster": {
      "Type": "AWS::ElastiCache::CacheCluster",
      "Properties": {
        "ClusterName": "cfn-cc",
        "CacheNodeType": "cache.t4g.micro",
        "Engine": "redis",
        "EngineVersion": "7.1",
        "NumCacheNodes": 1,
        "Port": 6379,
        "PreferredAvailabilityZone": "us-east-1a"
      }
    },
    "Repl": {
      "Type": "AWS::ElastiCache::ReplicationGroup",
      "Properties": {
        "ReplicationGroupId": "cfn-rg",
        "ReplicationGroupDescription": "from cfn",
        "CacheNodeType": "cache.t4g.micro",
        "Engine": "redis",
        "EngineVersion": "7.1",
        "NumCacheClusters": 2,
        "AutomaticFailoverEnabled": true,
        "MultiAZEnabled": true,
        "TransitEncryptionEnabled": true,
        "AtRestEncryptionEnabled": true,
        "Port": 6379
      }
    }
  },
  "Outputs": {
    "ClusterId": {"Value": {"Ref": "Cluster"}},
    "ReplId": {"Value": {"Ref": "Repl"}}
  }
}"#;

#[tokio::test]
async fn cfn_provisions_elasticache_cluster_and_replication_group() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let ec = aws_sdk_elasticache::Client::new(&server.aws_config().await);

    cfn.create_stack()
        .stack_name("ec-stack")
        .template_body(TEMPLATE)
        .capabilities(Capability::CapabilityIam)
        .on_failure(OnFailure::Rollback)
        .send()
        .await
        .expect("create_stack");

    // A cache cluster is only complete once its container is up, so the stack is IN_PROGRESS
    // until then (as a real CloudFormation stack is); wait for it instead of reading it at once.
    let stack = helpers::wait_until(std::time::Duration::from_secs(300), || async {
        let out = cfn
            .describe_stacks()
            .stack_name("ec-stack")
            .send()
            .await
            .ok()?;
        let s = out.stacks().first()?;
        (!s.stack_status()?.as_str().ends_with("IN_PROGRESS")).then(|| s.clone())
    })
    .await
    .expect("stack reached a terminal status");
    assert_eq!(stack.stack_status().unwrap().as_str(), "CREATE_COMPLETE");

    let cluster_resp = ec
        .describe_cache_clusters()
        .cache_cluster_id("cfn-cc")
        .send()
        .await
        .expect("describe_cache_clusters");
    let cluster = cluster_resp
        .cache_clusters()
        .first()
        .expect("cluster present");
    assert_eq!(cluster.cache_cluster_id(), Some("cfn-cc"));
    assert_eq!(cluster.engine(), Some("redis"));
    assert_eq!(cluster.cache_node_type(), Some("cache.t4g.micro"));

    let rg_resp = ec
        .describe_replication_groups()
        .replication_group_id("cfn-rg")
        .send()
        .await
        .expect("describe_replication_groups");
    let rg = rg_resp.replication_groups().first().expect("rg present");
    assert_eq!(rg.replication_group_id(), Some("cfn-rg"));
    assert_eq!(rg.description(), Some("from cfn"));
    assert_eq!(rg.transit_encryption_enabled(), Some(true));
    assert_eq!(rg.at_rest_encryption_enabled(), Some(true));

    cfn.delete_stack()
        .stack_name("ec-stack")
        .send()
        .await
        .expect("delete_stack");
}
