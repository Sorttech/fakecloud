//! CloudFormation generates names for resources whose name property the
//! template leaves out: `{StackName}-{LogicalId}-{SUFFIX}`, so two stacks from
//! one template never collide.

mod helpers;

use helpers::TestServer;

const TEMPLATE: &str = r#"{
  "Resources": {
    "Jobs": { "Type": "AWS::SQS::Queue" },
    "Ordered": { "Type": "AWS::SQS::Queue", "Properties": { "FifoQueue": true } },
    "Assets": { "Type": "AWS::S3::Bucket" },
    "Events": { "Type": "AWS::SNS::Topic" }
  },
  "Outputs": {
    "JobsUrl": { "Value": { "Ref": "Jobs" } },
    "OrderedUrl": { "Value": { "Ref": "Ordered" } },
    "Bucket": { "Value": { "Ref": "Assets" } },
    "Topic": { "Value": { "Ref": "Events" } }
  }
}"#;

async fn outputs(
    cfn: &aws_sdk_cloudformation::Client,
    stack: &str,
) -> std::collections::BTreeMap<String, String> {
    let described = cfn
        .describe_stacks()
        .stack_name(stack)
        .send()
        .await
        .unwrap();
    let stack = &described.stacks()[0];
    assert_eq!(
        stack.stack_status().map(|s| s.as_str()),
        Some("CREATE_COMPLETE"),
        "{:?}",
        stack.stack_status_reason()
    );
    stack
        .outputs()
        .iter()
        .map(|o| {
            (
                o.output_key().unwrap().to_string(),
                o.output_value().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn stacks_from_one_template_get_distinct_generated_names() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let sqs = server.sqs_client().await;
    let s3 = server.s3_client().await;

    for stack in ["dev-app", "prod-app"] {
        cfn.create_stack()
            .stack_name(stack)
            .template_body(TEMPLATE)
            .send()
            .await
            .unwrap_or_else(|e| panic!("create {stack}: {e:?}"));
    }
    let dev = outputs(&cfn, "dev-app").await;
    let prod = outputs(&cfn, "prod-app").await;

    for (key, value) in &dev {
        assert_ne!(Some(value), prod.get(key), "{key} collided across stacks");
    }

    let queue_name = |url: &str| url.rsplit('/').next().unwrap().to_string();
    let jobs = queue_name(&dev["JobsUrl"]);
    let (prefix, suffix) = jobs.rsplit_once('-').unwrap();
    assert_eq!(prefix, "dev-app-Jobs");
    assert_eq!(suffix.len(), 13, "{jobs}");

    let ordered = queue_name(&dev["OrderedUrl"]);
    assert!(ordered.starts_with("dev-app-Ordered-"), "{ordered}");
    assert!(ordered.ends_with(".fifo"), "{ordered}");
    let attrs = sqs
        .get_queue_attributes()
        .queue_url(&dev["OrderedUrl"])
        .attribute_names(aws_sdk_sqs::types::QueueAttributeName::FifoQueue)
        .send()
        .await
        .unwrap();
    assert_eq!(
        attrs
            .attributes()
            .and_then(|a| a.get(&aws_sdk_sqs::types::QueueAttributeName::FifoQueue))
            .map(String::as_str),
        Some("true")
    );

    // Bucket names are lowercase.
    let bucket = &dev["Bucket"];
    assert!(bucket.starts_with("dev-app-assets-"), "{bucket}");
    assert_eq!(bucket, &bucket.to_lowercase());
    s3.head_bucket().bucket(bucket).send().await.unwrap();

    assert!(
        dev["Topic"].contains(":dev-app-Events-"),
        "{}",
        dev["Topic"]
    );

    let queues = sqs.list_queues().send().await.unwrap();
    assert_eq!(queues.queue_urls().len(), 4, "{:?}", queues.queue_urls());
}

#[tokio::test]
async fn an_update_keeps_a_generated_name() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let sqs = server.sqs_client().await;

    let v1 = r#"{"Resources":{"Jobs":{"Type":"AWS::SQS::Queue"}},"Outputs":{"Url":{"Value":{"Ref":"Jobs"}}}}"#;
    let v2 = r#"{"Resources":{"Jobs":{"Type":"AWS::SQS::Queue","Properties":{"VisibilityTimeout":90}}},"Outputs":{"Url":{"Value":{"Ref":"Jobs"}}}}"#;
    cfn.create_stack()
        .stack_name("keep")
        .template_body(v1)
        .send()
        .await
        .unwrap();
    let before = outputs(&cfn, "keep").await["Url"].clone();
    cfn.update_stack()
        .stack_name("keep")
        .template_body(v2)
        .send()
        .await
        .unwrap();
    let described = cfn
        .describe_stacks()
        .stack_name("keep")
        .send()
        .await
        .unwrap();
    let stack = &described.stacks()[0];
    assert_eq!(
        stack.stack_status().map(|s| s.as_str()),
        Some("UPDATE_COMPLETE")
    );
    let after = stack
        .outputs()
        .iter()
        .find(|o| o.output_key() == Some("Url"))
        .and_then(|o| o.output_value())
        .unwrap();
    assert_eq!(before, after);
    let attrs = sqs
        .get_queue_attributes()
        .queue_url(after)
        .attribute_names(aws_sdk_sqs::types::QueueAttributeName::VisibilityTimeout)
        .send()
        .await
        .unwrap();
    assert_eq!(
        attrs
            .attributes()
            .and_then(|a| a.get(&aws_sdk_sqs::types::QueueAttributeName::VisibilityTimeout))
            .map(String::as_str),
        Some("90")
    );
}
