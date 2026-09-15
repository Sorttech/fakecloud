//! End-to-end tests for CloudFormation StackSets: stack instances are real
//! stacks, provisioned into their target account and region.

mod helpers;

use aws_credential_types::Credentials;
use aws_sdk_cloudformation::error::ProvideErrorMetadata;
use aws_sdk_cloudformation::types::{Parameter, StackInstanceStatus, StackSetOperationStatus};
use aws_sdk_sqs::types::QueueAttributeName;
use helpers::TestServer;

const DEFAULT_ACCOUNT: &str = "123456789012";
const MEMBER_ACCOUNT: &str = "222222222222";

const TEMPLATE: &str = r#"{
    "Parameters": {
        "Timeout": { "Type": "String", "Default": "30" }
    },
    "Resources": {
        "Queue": {
            "Type": "AWS::SQS::Queue",
            "Properties": {
                "QueueName": { "Fn::Sub": "${AWS::StackName}-queue" },
                "VisibilityTimeout": { "Ref": "Timeout" }
            }
        }
    }
}"#;

async fn member_sqs(server: &TestServer) -> aws_sdk_sqs::Client {
    let (akid, secret) = server.create_admin(MEMBER_ACCOUNT, "stackset-member").await;
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .endpoint_url(server.endpoint())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(akid, secret, None, None, "member"))
        .load()
        .await;
    aws_sdk_sqs::Client::new(&config)
}

async fn stack_set_queues(sqs: &aws_sdk_sqs::Client) -> Vec<String> {
    sqs.list_queues()
        .send()
        .await
        .unwrap()
        .queue_urls()
        .iter()
        .filter(|url| url.contains("StackSet-regional-"))
        .cloned()
        .collect()
}

/// Operations deploy in the background, as in AWS: poll until this one
/// finishes and return its final status.
async fn operation_status(
    cfn: &aws_sdk_cloudformation::Client,
    operation_id: &str,
) -> StackSetOperationStatus {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let status = cfn
            .describe_stack_set_operation()
            .stack_set_name("regional")
            .operation_id(operation_id)
            .send()
            .await
            .unwrap()
            .stack_set_operation()
            .and_then(|op| op.status())
            .cloned()
            .expect("operation status");
        if !matches!(
            status,
            StackSetOperationStatus::Running
                | StackSetOperationStatus::Queued
                | StackSetOperationStatus::Stopping
        ) {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "operation {operation_id} still {status:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn stack_set_instances_provision_stacks_across_accounts_and_regions() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let default_sqs = server.sqs_client().await;
    let member_sqs = member_sqs(&server).await;

    let stack_set_id = cfn
        .create_stack_set()
        .stack_set_name("regional")
        .template_body(TEMPLATE)
        .send()
        .await
        .unwrap()
        .stack_set_id()
        .expect("stack set id")
        .to_string();
    assert!(stack_set_id.starts_with("regional:"), "{stack_set_id}");

    let create = cfn
        .create_stack_instances()
        .stack_set_name("regional")
        .accounts(DEFAULT_ACCOUNT)
        .accounts(MEMBER_ACCOUNT)
        .regions("us-east-1")
        .regions("eu-west-1")
        .send()
        .await
        .unwrap();
    let create_op = create.operation_id().expect("operation id");
    assert_eq!(
        operation_status(&cfn, create_op).await,
        StackSetOperationStatus::Succeeded
    );

    // One real queue per region, in each target account.
    assert_eq!(stack_set_queues(&default_sqs).await.len(), 2);
    let member_queues = stack_set_queues(&member_sqs).await;
    assert_eq!(member_queues.len(), 2, "{member_queues:?}");

    let summaries = cfn
        .list_stack_instances()
        .stack_set_name("regional")
        .send()
        .await
        .unwrap();
    assert_eq!(summaries.summaries().len(), 4);
    for summary in summaries.summaries() {
        assert_eq!(summary.status(), Some(&StackInstanceStatus::Current));
        let stack_id = summary.stack_id().expect("stack id");
        assert!(
            stack_id.starts_with(&format!(
                "arn:aws:cloudformation:{}:{}:stack/StackSet-regional-",
                summary.region().unwrap(),
                summary.account().unwrap()
            )),
            "{stack_id}"
        );
    }

    // The default account's stacks are ordinary stacks it can describe.
    let default_instance = cfn
        .describe_stack_instance()
        .stack_set_name("regional")
        .stack_instance_account(DEFAULT_ACCOUNT)
        .stack_instance_region("eu-west-1")
        .send()
        .await
        .unwrap();
    let stack_id = default_instance
        .stack_instance()
        .and_then(|i| i.stack_id())
        .expect("stack id")
        .to_string();
    let stacks = cfn
        .describe_stacks()
        .stack_name(&stack_id)
        .send()
        .await
        .unwrap();
    assert_eq!(
        stacks.stacks()[0].stack_status().map(|s| s.as_str()),
        Some("CREATE_COMPLETE")
    );

    // Override a parameter for the member account's us-east-1 instance.
    let update = cfn
        .update_stack_instances()
        .stack_set_name("regional")
        .accounts(MEMBER_ACCOUNT)
        .regions("us-east-1")
        .parameter_overrides(
            Parameter::builder()
                .parameter_key("Timeout")
                .parameter_value("120")
                .build(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        operation_status(&cfn, update.operation_id().unwrap()).await,
        StackSetOperationStatus::Succeeded
    );
    let member_instance = cfn
        .describe_stack_instance()
        .stack_set_name("regional")
        .stack_instance_account(MEMBER_ACCOUNT)
        .stack_instance_region("us-east-1")
        .send()
        .await
        .unwrap();
    let overrides = member_instance
        .stack_instance()
        .unwrap()
        .parameter_overrides();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[0].parameter_value(), Some("120"));
    let member_stack_name = member_instance
        .stack_instance()
        .and_then(|i| i.stack_id())
        .and_then(|arn| arn.split('/').nth(1))
        .unwrap()
        .to_string();
    let queue_url = member_queues
        .iter()
        .find(|url| url.contains(&member_stack_name))
        .expect("queue of the overridden instance");
    let attrs = member_sqs
        .get_queue_attributes()
        .queue_url(queue_url)
        .attribute_names(QueueAttributeName::VisibilityTimeout)
        .send()
        .await
        .unwrap();
    assert_eq!(
        attrs
            .attributes()
            .and_then(|a| a.get(&QueueAttributeName::VisibilityTimeout))
            .map(String::as_str),
        Some("120")
    );

    // A stack set with instances cannot be deleted.
    let err = cfn
        .delete_stack_set()
        .stack_set_name("regional")
        .send()
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("StackSetNotEmptyException"));

    let delete = cfn
        .delete_stack_instances()
        .stack_set_name("regional")
        .accounts(DEFAULT_ACCOUNT)
        .accounts(MEMBER_ACCOUNT)
        .regions("us-east-1")
        .regions("eu-west-1")
        .retain_stacks(false)
        .send()
        .await
        .unwrap();
    assert_eq!(
        operation_status(&cfn, delete.operation_id().unwrap()).await,
        StackSetOperationStatus::Succeeded
    );
    assert!(stack_set_queues(&default_sqs).await.is_empty());
    assert!(stack_set_queues(&member_sqs).await.is_empty());

    let operations = cfn
        .list_stack_set_operations()
        .stack_set_name("regional")
        .send()
        .await
        .unwrap();
    assert_eq!(operations.summaries().len(), 3);

    cfn.delete_stack_set()
        .stack_set_name("regional")
        .send()
        .await
        .unwrap();
    let err = cfn
        .describe_stack_set()
        .stack_set_name("regional")
        .send()
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("StackSetNotFoundException"));
}

#[tokio::test]
async fn stack_set_update_redeploys_instances() {
    let server = TestServer::start().await;
    let cfn = server.cloudformation_client().await;
    let sqs = server.sqs_client().await;

    cfn.create_stack_set()
        .stack_set_name("regional")
        .template_body(TEMPLATE)
        .send()
        .await
        .unwrap();
    let create = cfn
        .create_stack_instances()
        .stack_set_name("regional")
        .accounts(DEFAULT_ACCOUNT)
        .regions("us-east-1")
        .send()
        .await
        .unwrap();
    assert_eq!(
        operation_status(&cfn, create.operation_id().unwrap()).await,
        StackSetOperationStatus::Succeeded
    );

    let update = cfn
        .update_stack_set()
        .stack_set_name("regional")
        .use_previous_template(true)
        .parameters(
            Parameter::builder()
                .parameter_key("Timeout")
                .parameter_value("45")
                .build(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        operation_status(&cfn, update.operation_id().unwrap()).await,
        StackSetOperationStatus::Succeeded
    );

    let queues = stack_set_queues(&sqs).await;
    assert_eq!(queues.len(), 1);
    let attrs = sqs
        .get_queue_attributes()
        .queue_url(&queues[0])
        .attribute_names(QueueAttributeName::VisibilityTimeout)
        .send()
        .await
        .unwrap();
    assert_eq!(
        attrs
            .attributes()
            .and_then(|a| a.get(&QueueAttributeName::VisibilityTimeout))
            .map(String::as_str),
        Some("45")
    );

    let described = cfn
        .describe_stack_set()
        .stack_set_name("regional")
        .send()
        .await
        .unwrap();
    let set = described.stack_set().unwrap();
    assert_eq!(set.parameters()[0].parameter_value(), Some("45"));
    assert_eq!(set.regions(), ["us-east-1"]);
}
