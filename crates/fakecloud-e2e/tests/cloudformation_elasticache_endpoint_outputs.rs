//! A container-backed `AWS::ElastiCache::CacheCluster` must not let its stack reach CREATE_COMPLETE
//! (or resolve its Outputs, Exports and dependent resources) with the pre-container placeholder
//! endpoint. CloudFormation finishes a cache cluster once it is available, so
//! `Fn::GetAtt Cache.RedisEndpoint.Address` / `.Port` carry the endpoint
//! DescribeCacheClusters reports
//! (<https://docs.aws.amazon.com/AWSCloudFormation/latest/UserGuide/aws-resource-elasticache-cachecluster.md>).
//! Before this the attributes were fixed at create time to
//! `<id>.fakecloud.<region>.cache.amazonaws.com` and port 6379 while the container was really on
//! `host.docker.internal` and a published port, so a task handed the attribute dialled a host that
//! does not resolve, or the wrong port.
//!
//! Covers CreateStack and the change-set path `cdk deploy` uses. With no container runtime the
//! cluster is `available` at insert and the endpoint never moves, so the assertions hold trivially
//! there.

mod helpers;

use aws_sdk_cloudformation::types::{ChangeSetType, Stack};
use helpers::TestServer;

fn template(id: &str) -> String {
    format!(
        r#"{{
  "Resources": {{
    "Cache": {{
      "Type": "AWS::ElastiCache::CacheCluster",
      "Properties": {{
        "ClusterName": "{id}",
        "CacheNodeType": "cache.t4g.micro",
        "Engine": "redis",
        "NumCacheNodes": 1
      }}
    }},
    "Host": {{
      "Type": "AWS::SSM::Parameter",
      "Properties": {{
        "Name": "/{id}/host",
        "Type": "String",
        "Value": {{"Fn::GetAtt": ["Cache", "RedisEndpoint.Address"]}}
      }}
    }}
  }},
  "Outputs": {{
    "Address": {{
      "Value": {{"Fn::GetAtt": ["Cache", "RedisEndpoint.Address"]}},
      "Export": {{"Name": "{id}-address"}}
    }},
    "Port": {{
      "Value": {{"Fn::GetAtt": ["Cache", "RedisEndpoint.Port"]}},
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

/// Once the stack is complete, the cluster is available and every value derived from its endpoint
/// matches DescribeCacheClusters.
async fn assert_real_endpoint(server: &TestServer, stack_name: &str, id: &str) {
    let cfn = server.cloudformation_client().await;
    let elasticache = server.elasticache_client().await;
    let ssm = server.ssm_client().await;

    let stack = wait_stack_terminal(&cfn, stack_name).await;
    assert_eq!(
        stack.stack_status().map(|s| s.as_str()),
        Some("CREATE_COMPLETE"),
        "reason: {:?}",
        stack.stack_status_reason()
    );

    let described = elasticache
        .describe_cache_clusters()
        .cache_cluster_id(id)
        .show_cache_node_info(true)
        .send()
        .await
        .expect("describe_cache_clusters");
    let cluster = described.cache_clusters().first().expect("cluster present");
    assert_eq!(cluster.cache_cluster_status(), Some("available"));
    let endpoint = cluster.cache_nodes()[0].endpoint().expect("endpoint");
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
        "same-stack consumer of RedisEndpoint.Address"
    );

    cfn.delete_stack()
        .stack_name(stack_name)
        .send()
        .await
        .expect("delete_stack");
}

#[tokio::test]
async fn create_stack_outputs_carry_the_real_redis_endpoint() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    cfn.create_stack()
        .stack_name("ec-endpoint-create")
        .template_body(template("ec-endpoint-create"))
        .send()
        .await
        .expect("create_stack");
    assert_real_endpoint(&server, "ec-endpoint-create", "ec-endpoint-create").await;
}

#[tokio::test]
async fn execute_change_set_outputs_carry_the_real_redis_endpoint() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let cs = cfn
        .create_change_set()
        .stack_name("ec-endpoint-cs")
        .change_set_name("create")
        .change_set_type(ChangeSetType::Create)
        .template_body(template("ec-endpoint-cs"))
        .send()
        .await
        .expect("create_change_set");
    cfn.execute_change_set()
        .change_set_name(cs.id().expect("change set id"))
        .send()
        .await
        .expect("execute_change_set");
    assert_real_endpoint(&server, "ec-endpoint-cs", "ec-endpoint-cs").await;
}
